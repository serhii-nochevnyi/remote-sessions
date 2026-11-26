#!/usr/bin/env python3
"""Run the installed terminal pressure fixture in its own logged-in Mac lab.

Build host-agent and the debug probe from the current committed source first.
For native acceptance, pass --owner-app and --victim-app with two separately
signed debug copies of that build, using distinct bundle IDs. This runner opens
the actual binaries and prints private coordination paths and the evidence
format. The operator uses the native inspector in the victim only. Write each
operator JSON record atomically with mode 0600. Times ending in _ns are from
time.monotonic_ns() on this Mac. Keep screenshots and terminal text outside
these records. Use --describe-native to print the operator sequence and schema.
"""
import argparse
from contextlib import ExitStack, closing
import hashlib
from itertools import count
import json
import os
from pathlib import Path
import plistlib
import re
import shutil
import sqlite3
import stat
import subprocess
import sys
import termios
import threading
import time
import tty
import uuid

ROOT = Path(__file__).resolve().parents[2]
CAPACITY = 16
BRIDGE_INPUT = b'RS-BRIDGE-FRESH\r'
NATIVE_INPUT = b'RS-NATIVE-FRESH\r'
PREFIX = b'RS_W1_RESIZE_'
PLAN_PATHS = (
    'crates/desktop-bridge/src/lib.rs', 'crates/desktop-bridge/src/w1_terminal_tests.rs',
    'apps/client/src/TerminalView.tsx', 'apps/client/src/local-client.ts',
    'apps/client/src/local-client.test.ts', 'crates/desktop-bridge/examples/probe.rs',
    'tests/integration/w1_terminal_pressure.py',
)
BURSTS = {'startup': 12, 'bridge': 320, 'native': 750}
MARKERS = {'startup': 'RSSTARTREADY', 'bridge': 'RSBRIDGEKEPT', 'native': 'RSNATIVEKEPT'}
MAX_RECORD = 65_536
MAX_PROVIDER_STATES = 4096
PUBLICATION_WAIT_SECONDS = 0.25
NATIVE_OWNER_SETUP_SECONDS = 1800
NATIVE_INSPECTOR_SETUP_SECONDS = 1800
NATIVE_FINAL_RECORD_SECONDS = 7200
NATIVE_FIXTURE_GRACE_SECONDS = 60
CLEANUP_NS = 2_000_000_000
PS_START = re.compile(r'[A-Z][a-z]{2} [A-Z][a-z]{2}\s[ 0-9][0-9] \d\d:\d\d:\d\d \d{4}')


class ProofError(Exception):
    """An evidence failure whose fixed message is safe to publish."""


def need(condition, message):
    if not condition:
        raise ProofError(message)


def ns():
    return time.monotonic_ns()


def exact(value, names, message='unexpected evidence fields'):
    need(isinstance(value, dict) and set(value) == set(names.split()), message)
    return value


def positive(value):
    return type(value) is int and 0 < value <= 2**63 - 1


def uuid_text(value):
    try:
        parsed = uuid.UUID(value)
        return parsed.int != 0 and str(parsed) == value
    except (ValueError, TypeError, AttributeError):
        return False


def size(value):
    exact(value, 'cols rows', 'invalid grid shape')
    need(type(value['cols']) is int and 2 <= value['cols'] <= 500
         and type(value['rows']) is int and 2 <= value['rows'] <= 200, 'invalid grid size')
    return value


def epoch(value):
    exact(value, 'agent owner', 'invalid ownership scope')
    need(all(uuid_text(item) for item in value.values()), 'invalid ownership identity')
    return value


def target(value):
    exact(value, 'runtime_id generation session window pane pid process_start', 'invalid runtime target')
    need(uuid_text(value['runtime_id']) and uuid_text(value['generation'])
         and positive(value['pid']) and positive(value['process_start']), 'invalid runtime identity')
    for key, prefix in (('session', r'\$'), ('window', '@'), ('pane', '%')):
        need(isinstance(value[key], str) and re.fullmatch(prefix + r'(0|[1-9][0-9]{0,9})', value[key]),
             'invalid tmux target')
    return value


def status(value):
    exact(value, 'terminal epoch committed provider observation', 'invalid status evidence')
    scope = exact(value['terminal'], 'target view_id agent_epoch')
    target(scope['target'])
    need(uuid_text(scope['view_id']) and uuid_text(scope['agent_epoch']), 'invalid view scope')
    if value['epoch'] is not None:
        need(epoch(value['epoch'])['agent'] == scope['agent_epoch'], 'status changed agent epoch')
    committed = exact(value['committed'], 'size revision intent')
    size(committed['size'])
    revision = committed['revision']
    need(type(revision) is int and 0 <= revision <= 2**53 - 1, 'invalid committed revision')
    if revision:
        intent = exact(committed['intent'], 'id epoch')
        need(uuid_text(intent['id']) and epoch(intent['epoch'])['agent'] == scope['agent_epoch'],
             'invalid committed intent')
    else:
        need(committed['intent'] is None, 'unexpected initial resize intent')
    if value['provider'] is not None:
        grid_sample(value['provider'])
    need(value['observation'] in ('observed', 'pending', 'unavailable'), 'invalid provider observation')
    return value


def grid_sample(value):
    exact(value, 'size sampled_at_us')
    size(value['size'])
    need(positive(value['sampled_at_us']), 'invalid grid sample time')
    return value


def capture_json(path):
    invalid = 'record must be a bounded private file'
    deadline = time.monotonic() + PUBLICATION_WAIT_SECONDS
    while True:
        descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(descriptor, 'rb') as stream:
            while True:
                meta = os.fstat(stream.fileno())
                need(stat.S_ISREG(meta.st_mode) and stat.S_IMODE(meta.st_mode) == 0o600
                     and meta.st_uid == os.getuid()
                     and 0 < meta.st_size <= MAX_RECORD, invalid)
                if meta.st_nlink in (0, 1):
                    break
                remaining = deadline - time.monotonic()
                need(meta.st_nlink == 2 and remaining > 0, invalid)
                time.sleep(min(0.01, remaining))
            if meta.st_nlink == 1:
                data = stream.read(MAX_RECORD + 1)
                break
        need(time.monotonic() < deadline, invalid)
    need(len(data) == meta.st_size, 'record changed during read')

    def unique(pairs):
        fields = dict(pairs)
        need(len(fields) == len(pairs), 'duplicate evidence key')
        return fields

    return json.loads(data, object_pairs_hook=unique)


def write_json(path, record, replace=False):
    pending = path.with_name(path.name + '.tmp')
    with private_writer(pending) as output:
        json.dump(record, output, separators=(',', ':'))
        output.write('\n')
    if replace:
        pending.replace(path)
    else:
        try:
            os.link(pending, path)
        finally:
            pending.unlink()


def private_writer(path):
    return os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600), 'w')


def test_record_publication():
    from tempfile import TemporaryDirectory
    from unittest.mock import patch

    with TemporaryDirectory() as directory:
        path = Path(directory) / 'record.json'
        pending = path.with_name(path.name + '.tmp')
        record = {'sequence': 1}
        write_json(pending, record)
        os.link(pending, path)
        need(path.stat().st_nlink == 2, 'publication test requires two links')

        with patch.object(time, 'monotonic', return_value=0), \
                patch.object(time, 'sleep', side_effect=lambda _: pending.unlink()) as pause:
            need(capture_json(path) == record, 'publication changed record contents')
            need(pause.call_count == 1, 'reader did not wait for publication')

        try:
            write_json(path, {'sequence': 2})
        except FileExistsError:
            pass
        else:
            raise ProofError('publication overwrote an existing record')
        need(capture_json(path) == record and not pending.exists(), 'failed publication changed record')

        replacement = {'sequence': 2}
        fstat = os.fstat
        observed_links = []

        def replace_before_fstat(descriptor):
            if not observed_links:
                write_json(path, replacement, replace=True)
            metadata = fstat(descriptor)
            observed_links.append(metadata.st_nlink)
            return metadata

        with patch.object(os, 'fstat', side_effect=replace_before_fstat):
            need(capture_json(path) == replacement, 'reader did not capture the replacement')
        need(observed_links == [0, 1], 'reader did not reopen the unlinked generation')

        os.link(path, pending)
        with patch.object(time, 'monotonic', side_effect=(0, PUBLICATION_WAIT_SECONDS)):
            try:
                capture_json(path)
            except ProofError as error:
                need(str(error) == 'record must be a bounded private file', 'unexpected rejection')
            else:
                raise ProofError('reader accepted a persistent hard link')


def wait(check, seconds, message):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        result = check()
        if result:
            return result
        time.sleep(0.02)
    raise ProofError(message)


def read_if_present(path):
    try:
        return capture_json(path)
    except FileNotFoundError:
        return None


def run_command(*args, timeout=10, check=True):
    result = subprocess.run([str(arg) for arg in args], stdin=subprocess.DEVNULL,
                            capture_output=True, text=True, timeout=timeout, env=dict(os.environ, LC_ALL='C'))
    if check:
        need(result.returncode == 0, f'{Path(args[0]).name} command failed')
    return result


def output(*args):
    return run_command(*args).stdout.strip()


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def process(pid):
    need(positive(pid) and pid <= 2**31 - 1, 'invalid process ID')
    line = run_command('/bin/ps', '-p', pid, '-o', 'rss=', '-o', 'lstart=', check=False).stdout.strip()
    if not line:
        return None
    rss, started = line.split(None, 1)
    need(rss.isdecimal() and PS_START.fullmatch(started), 'unexpected process sample')
    return {'pid': pid, 'started': started, 'rss_kib': int(rss), 'sampled_at_ns': ns()}


def live_process(pid):
    current = process(pid)
    need(current is not None and current['rss_kib'] > 0, 'expected live process sample')
    return current


def same_process(first, second):
    return second is not None and (first['pid'], first['started']) == (second['pid'], second['started'])


def versions(probe, host, tmux):
    source = output('/usr/bin/git', '-C', ROOT, 'rev-parse', 'HEAD', 'HEAD^{tree}').splitlines()
    need(len(source) == 2 and all(re.fullmatch(r'[0-9a-f]{40}', item) for item in source), 'invalid source identity')
    need(run_command('/usr/bin/git', '-C', ROOT, 'diff', '--quiet', 'HEAD', '--', *PLAN_PATHS,
                     check=False).returncode == 0, 'pressure source differs from committed tree')
    return {
        'source': source[0], 'tree': source[1],
        'probe_sha256': digest(probe), 'host_sha256': digest(host), 'tmux_sha256': digest(Path(tmux)),
        'tmux': output(tmux, '-V'), 'macos': output('/usr/bin/sw_vers', '-productVersion'),
        'macos_build': output('/usr/bin/sw_vers', '-buildVersion'),
        'machine': output('/usr/bin/uname', '-m'), 'rustc': output('rustc', '--version'),
        'python': f'{sys.version_info.major}.{sys.version_info.minor}.{sys.version_info.micro}',
    }


def sample_clock():
    before = ns()
    wall_us = time.time_ns() // 1000
    after = ns()
    return {'mono_ns': (before + after) // 2, 'wall_us': wall_us}


def mono(at_us, anchor):
    need(positive(at_us), 'invalid observation timestamp')
    return anchor['mono_ns'] + (at_us - anchor['wall_us']) * 1000


def fixture(state):
    tty.setraw(sys.stdin.fileno(), termios.TCSANOW)
    instance = str(uuid.uuid4())

    def heartbeat():
        for sequence in count():
            write_json(state / 'pressure-heartbeat.json',
                       {'pid': os.getpid(), 'instance': instance, 'sequence': sequence, 'at_ns': ns()}, replace=True)
            time.sleep(0.025)

    def input_ledger():
        descriptor = os.open(state / 'pressure-input.bin', os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, 'wb', buffering=0) as ledger:
            total = 0
            while total <= 4096:
                data = os.read(sys.stdin.fileno(), 256)
                if not data:
                    return
                total += ledger.write(data)

    def render(data):
        sys.stdout.buffer.write(data)
        sys.stdout.buffer.flush()

    def flood(leg):
        started = ns()
        total = 0
        for burst in range(BURSTS[leg]):
            rows = [bytes([65 + ((row + burst) % 26)]) * 70 for row in range(24)]
            data = b'\x1b[H' + b'\r\n'.join(rows) + f'\r\nRSPROGRESS{burst:04d}'.encode()
            render(data)
            total += len(data)
            time.sleep(0.012)
        finish = b'\x1b[2J\x1b[H' + MARKERS[leg].encode()
        render(finish)
        write_json(state / f'pressure-{leg}.json', {
            'leg': leg, 'started_ns': started, 'finished_ns': ns(), 'bytes': total + len(finish),
            'bursts': BURSTS[leg], 'marker': MARKERS[leg],
        })

    threading.Thread(target=heartbeat, daemon=True).start()
    threading.Thread(target=input_ledger, daemon=True).start()
    flood('startup')
    trigger_budgets = {
        'bridge': 1200,
        'native': NATIVE_OWNER_SETUP_SECONDS + NATIVE_INSPECTOR_SETUP_SECONDS + NATIVE_FIXTURE_GRACE_SECONDS,
    }
    for leg, seconds in trigger_budgets.items():
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if (state / f'pressure-{leg}.trigger').exists():
                flood(leg)
                break
            time.sleep(0.05)
        else:
            return
    # Keep the marker on screen after the last burst. Never re-emit it.
    threading.Event().wait(NATIVE_FINAL_RECORD_SECONDS + NATIVE_FIXTURE_GRACE_SECONDS)


def workload(state, leg):
    result = exact(capture_json(state / f'pressure-{leg}.json'),
                   'leg started_ns finished_ns bytes bursts marker')
    need(result['leg'] == leg and result['bursts'] == BURSTS[leg] and result['marker'] == MARKERS[leg]
         and positive(result['bytes']) and result['bytes'] >= result['bursts'] * 1600
         and positive(result['started_ns']) and result['started_ns'] < result['finished_ns'] <= ns(),
         'finite flood start or finish missing')
    return result


def heartbeat(state, pid):
    current = exact(capture_json(state / 'pressure-heartbeat.json'), 'pid instance sequence at_ns')
    need(current['pid'] == pid and uuid_text(current['instance']) and positive(current['sequence'])
         and positive(current['at_ns']) and current['at_ns'] <= ns(), 'invalid heartbeat evidence')
    return current


def check_heartbeat(before, after, at):
    need(before['pid'] == after['pid'] and before['instance'] == after['instance']
         and before['sequence'] < after['sequence'] and before['at_ns'] <= at < after['at_ns'],
         'independent heartbeat did not advance across Full')


def ledger(state, expected):
    path = state / 'pressure-input.bin'
    with path.open('rb') as stream:
        actual = stream.read(len(expected) + 1)
    need(actual == expected, 'raw fixture input mismatch or replay')
    return {'exact': True, 'fresh_count': 1, 'stale_count': 0}


class Trace:
    """Read only the fixed debug record types from an owned app/probe log."""

    def __init__(self, path):
        self.path = path
        self.offset = 0
        self.pressure = {}
        self.grids = []
        self._providers = {}
        self.failures = []

    def provider_samples(self):
        for first, latest in self._providers.values():
            yield latest
            if first is not latest:
                yield first

    def _retain_provider(self, item):
        provider = item['provider']
        identity = json.dumps({**item, 'provider': provider['size'] if provider is not None else None},
                              sort_keys=True, separators=(',', ':'))
        if identity not in self._providers:
            need(len(self._providers) < MAX_PROVIDER_STATES, 'too many debug observations')
            self._providers[identity] = (item, item)
        elif provider is not None:
            first, latest = self._providers[identity]
            sampled_at = provider['sampled_at_us']
            if sampled_at < first['provider']['sampled_at_us']:
                self._providers[identity] = (item, latest)
            elif sampled_at > latest['provider']['sampled_at_us']:
                self._providers[identity] = (first, item)

    def read(self):
        with self.path.open('rb') as stream:
            stream.seek(self.offset)
            for _ in range(8192):
                line = stream.readline(MAX_RECORD + 1)
                if not line or not line.endswith(b'\n'):
                    need(len(line) < MAX_RECORD, 'oversized native diagnostic record')
                    break
                need(len(line) <= MAX_RECORD, 'oversized native diagnostic record')
                self.offset = stream.tell()
                if not line.startswith(PREFIX):
                    continue
                kind, _, payload = line[len(PREFIX):].partition(b' ')
                if kind not in (b'PRESSURE', b'GRID', b'PROVIDER', b'FAILURE'):
                    continue
                item = json.loads(payload)
                need(isinstance(item, dict), 'invalid diagnostic payload')
                if kind == b'PRESSURE':
                    fields = 'event view_id pid at_us'
                    event = item.get('event')
                    if event == 'full':
                        fields += ' high_water'
                    exact(item, fields)
                    need(event in ('attached', 'full', 'detached', 'reader_exit', 'child_reaped', 'cleanup_complete', 'cleanup_timeout')
                         and uuid_text(item['view_id']) and positive(item['pid']) and positive(item['at_us'])
                         and (event != 'full' or item['high_water'] == CAPACITY), 'invalid producer witness')
                    events = self.pressure.setdefault(item['view_id'], {})
                    need(event not in events, 'duplicate view retirement or diagnostic')
                    events[event] = item
                elif kind == b'GRID':
                    self.grids.append(grid_observation(item))
                elif kind == b'PROVIDER':
                    self._retain_provider(status(item))
                else:
                    exact(item, 'view_id stage reason')
                    if item['reason'] == 'stream_closed' and item['stage'] in ('read', 'renderer_detach'):
                        need(uuid_text(item['view_id']), 'invalid stream closure identity')
                        self.failures.append(item)
        need(len(self.pressure) <= 128 and len(self.grids) <= 256
             and len(self.failures) <= 256, 'too many debug observations')
        return self


def grid_observation(item):
    exact(item, 'event renderer')
    event = exact(item['event'], 'status bridge_pty client')
    observed = status(event['status'])
    bridge = grid_sample(event['bridge_pty'])
    client = grid_sample(event['client'])
    renderer = exact(item['renderer'], 'renderer_id epoch revision size focused')
    need(uuid_text(renderer['renderer_id']) and type(renderer['focused']) is bool
         and renderer['epoch'] == observed['epoch'] and renderer['revision'] == observed['committed']['revision']
         and size(renderer['size']) == bridge['size'] == client['size'] == observed['committed']['size'],
         'renderer and bridge observations lack exact scope or grid')
    return item


def successor_provider_observed(trace, expected, view_id, successor, anchor, detached, fresh):
    return any(sample['terminal']['view_id'] == view_id and sample['terminal']['target'] == expected
               and sample['epoch'] == successor and sample['provider'] is not None
               and detached < mono(sample['provider']['sampled_at_us'], anchor) <= fresh
               for sample in trace.provider_samples())


def test_trace_retention():
    from copy import deepcopy
    from tempfile import TemporaryDirectory

    def identity(number):
        return str(uuid.UUID(int=number))

    grid = {'cols': 80, 'rows': 24}
    expected = {'runtime_id': identity(1), 'generation': identity(2), 'session': '$1',
                'window': '@1', 'pane': '%1', 'pid': 100, 'process_start': 101}
    view = identity(3)
    successor = {'agent': identity(4), 'owner': identity(5)}

    def provider(at_us):
        return {'terminal': {'target': dict(expected), 'view_id': view, 'agent_epoch': successor['agent']},
                'epoch': dict(successor), 'committed': {'size': dict(grid), 'revision': 1,
                                                        'intent': {'id': identity(6), 'epoch': dict(successor)}},
                'provider': {'size': dict(grid), 'sampled_at_us': at_us}, 'observation': 'observed'}

    def line(kind, item):
        return PREFIX + kind + b' ' + json.dumps(item).encode() + b'\n'

    def append(path, lines):
        with path.open('ab') as log:
            log.writelines(lines)

    def rejected(trace, message):
        try:
            trace.read()
        except ProofError as error:
            need(str(error) == message, 'unexpected diagnostic rejection')
        else:
            raise ProofError('reader accepted invalid diagnostic evidence')

    with TemporaryDirectory() as directory:
        path = Path(directory) / 'trace.log'
        trace = Trace(path)
        attached = {'event': 'attached', 'view_id': view, 'pid': 102, 'at_us': 95}
        full = {**attached, 'event': 'full', 'at_us': 97, 'high_water': CAPACITY}
        grid_event = {'event': {'status': provider(100), 'bridge_pty': {'size': grid, 'sampled_at_us': 98},
                                'client': {'size': grid, 'sampled_at_us': 99}},
                      'renderer': {'renderer_id': identity(7), 'epoch': successor,
                                   'revision': 1, 'size': grid, 'focused': True}}
        append(path, [line(b'PRESSURE', attached), line(b'PRESSURE', full), line(b'GRID', grid_event),
                      line(b'PROVIDER', provider(100)), line(b'PROVIDER', provider(101))])
        captured = {'grid': trace.read().grids[0], 'provider': next(trace.provider_samples())}
        original = deepcopy(captured)
        for first in (102, 3102, 6102):
            append(path, (line(b'PROVIDER', provider(at_us)) for at_us in range(first, first + 3000)))
            trace.read()
        append(path, [line(b'PROVIDER', provider(110)), PREFIX + b'UNKNOWN invalid-json\n'])
        retained = list(trace.read().provider_samples())
        need(retained == [provider(9101), provider(100)], 'equivalent traffic lost provider endpoints')
        need(captured == original and captured['provider'] == provider(101), 'captured pressure witness changed')
        need(trace.grids == [grid_event] and trace.pressure == {view: {'attached': attached, 'full': full}},
             'provider retention changed independent diagnostics')
        anchor = {'wall_us': 100, 'mono_ns': 100_000}
        need(successor_provider_observed(trace, expected, view, successor, anchor, 99_000, 102_000)
             and not successor_provider_observed(trace, expected, view, successor, anchor, 89_000, 98_000),
             'historical successor status is missing or crossed the handoff window')

        reordered = json.loads(json.dumps(provider(9101), sort_keys=True))
        append(path, [line(b'PROVIDER', reordered), line(b'PROVIDER', provider(96))])
        need(list(trace.read().provider_samples()) == [provider(9101), provider(96)],
             'key order or delayed timestamps changed endpoint selection')

        variants = []
        for keys, value in (
            (('terminal', 'target', 'runtime_id'), identity(13)),
            (('terminal', 'target', 'generation'), identity(14)),
            (('terminal', 'target', 'session'), '$2'), (('terminal', 'target', 'window'), '@2'),
            (('terminal', 'target', 'pane'), '%2'), (('terminal', 'target', 'pid'), 103),
            (('terminal', 'target', 'process_start'), 200), (('terminal', 'view_id'), identity(8)),
            (('epoch', 'owner'), identity(9)), (('committed', 'revision'), 2),
            (('committed', 'size', 'cols'), 81), (('committed', 'intent', 'id'), identity(10)),
            (('committed', 'intent', 'epoch', 'owner'), identity(11)),
            (('provider', 'size', 'cols'), 82), (('provider',), None), (('observation',), 'pending'),
        ):
            item = deepcopy(provider(9200))
            field = item
            for key in keys[:-1]:
                field = field[key]
            field[keys[-1]] = value
            variants.append(item)
        agent_changed = deepcopy(provider(9200))
        agent_changed['terminal']['agent_epoch'] = identity(12)
        agent_changed['epoch']['agent'] = identity(12)
        agent_changed['committed']['intent']['epoch']['agent'] = identity(12)
        variants.append(agent_changed)
        append(path, (line(b'PROVIDER', item) for item in variants))
        need(list(trace.read().provider_samples()) == [provider(9101), provider(96), *variants],
             'distinct provider status identities were combined')

        payload = line(b'PROVIDER', provider(101)).rstrip(b'\n')
        for bad, message in (
            (line(b'PROVIDER', provider(0)), 'invalid grid sample time'),
            (payload.ljust(MAX_RECORD, b' ') + b'\n', 'oversized native diagnostic record'),
            (line(b'PRESSURE', full), 'duplicate view retirement or diagnostic'),
        ):
            invalid = Trace(Path(directory) / 'invalid.log')
            invalid.path.write_bytes(line(b'PROVIDER', provider(100)) + line(b'PRESSURE', full))
            invalid.read()
            append(invalid.path, [bad])
            rejected(invalid, message)

        capacity = Trace(Path(directory) / 'capacity.log')
        with capacity.path.open('wb') as log:
            for revision in range(1, MAX_PROVIDER_STATES + 2):
                item = provider(100)
                item['committed']['revision'] = revision
                log.write(line(b'PROVIDER', item))
        rejected(capacity, 'too many debug observations')
        need(sum(1 for _ in capacity.provider_samples()) == MAX_PROVIDER_STATES,
             'distinct provider states exceeded their budget')


def full_cleanup(trace, view_id, pid, anchor):
    events = trace.pressure.get(view_id, {})
    required = ('attached', 'full', 'reader_exit', 'child_reaped', 'cleanup_complete')
    if not all(name in events for name in required):
        return None
    need('cleanup_timeout' not in events and all(events[name]['pid'] == pid for name in required),
         'retirement exceeded budget or changed attachment client')
    times = {name: mono(events[name]['at_us'], anchor) for name in required}
    need(times['attached'] <= times['full'] <= min(times['reader_exit'], times['child_reaped'])
         <= max(times['reader_exit'], times['child_reaped']) <= times['cleanup_complete']
         <= times['full'] + CLEANUP_NS, 'producer and cleanup ordering is incomplete')
    return {'events': {name: events[name] for name in required}, 'times_ns': times}


def tmux_target(lab, expected):
    fields = '#{session_id}|#{window_id}|#{pane_id}|#{pane_pid}|#{@rs_create_generation}|#{pane_dead}|#{window_width}|#{window_height}'
    result = lab.tmux_call('display-message', '-p', '-t', expected['pane'], fields)
    parts = result.stdout.strip().split('|')
    need(result.returncode == 0 and len(parts) == 8, 'exact tmux target unavailable')
    need(parts[:6] == [expected['session'], expected['window'], expected['pane'], str(expected['pid']),
                       expected['generation'], '0'], 'tmux target changed')
    return size({'cols': int(parts[6]), 'rows': int(parts[7])})


def tmux_server(lab):
    result = lab.tmux_call('display-message', '-p', '#{pid}|#{start_time}|#{@rs_create_generation}')
    need(result.returncode == 0, 'dedicated tmux process unavailable')
    pid, started, generation = result.stdout.strip().split('|')
    need(pid.isdecimal() and started.isdecimal() and uuid_text(generation), 'invalid tmux generation')
    return {'process': live_process(int(pid)), 'started': int(started), 'generation': generation}


def tmux_client(lab, expected, pid):
    result = lab.tmux_call('list-clients', '-t', expected['session'], '-F',
                           '#{client_pid}|#{session_id}|#{window_id}|#{pane_id}|#{client_flags}')
    scope = [str(pid), expected['session'], expected['window'], expected['pane']]
    matches = [line.split('|') for line in result.stdout.splitlines() if line.split('|')[:4] == scope]
    need(result.returncode == 0 and len(matches) == 1 and len(matches[0]) == 5
         and 'ignore-size' in matches[0][4].split(','), 'exact disposable attachment was not observed')


def bridge_leg(lab, probe, runtime, report, anchor):
    trace_path = lab.state / 'pressure-bridge.log'
    with private_writer(trace_path) as trace:
        environment = dict(os.environ, LC_ALL='C', RS_W1_RESIZE_TRACE='1')
        result = subprocess.run([str(probe), 'pressure', '--run-id', report['run_id'], '--state-dir', str(lab.state),
                                 '--runtime-id', runtime['runtime_id'], '--tmux-bin', lab.tmux,
                                 '--report', str(lab.state / 'pressure-probe.json')],
                                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=trace,
                                env=environment, timeout=50)
    if result.returncode:
        trace = Trace(trace_path).read()
        if not any('full' in events for events in trace.pressure.values()):
            report['bridge'] = {'outcome': 'overflow not reached'}
            raise ProofError('overflow not reached in bridge leg')
        raise ProofError('bridge pressure probe failed after producer Full')
    witness = exact(capture_json(lab.state / 'pressure-probe.json'),
                    'run_id agent_pid source source_tree engine_version started_at_us ended_at_us '
                    'overflow_returned_at_us elapsed_ms owner victim replacement heartbeat_before heartbeat_after '
                    'committed owner_grid provider_after fresh_status healthy_output startup_redraw fresh_redraw '
                    'fresh_input_submitted closure')
    tools = report['build']
    need(witness['run_id'] == report['run_id'] and witness['agent_pid'] == report['agent']['pid']
         and (witness['source'], witness['source_tree'], witness['engine_version'])
         == (tools['source'], tools['tree'], tools['tmux']), 'probe changed run, agent, source or engine')
    progress = exact(witness['healthy_output'], 'bytes first_at_us last_at_us')
    need(witness['startup_redraw'] is True and witness['fresh_redraw'] is True
         and witness['fresh_input_submitted'] is True and progress['bytes'] > 8192
         and witness['closure'] == {'reason': 'overflow', 'high_water': CAPACITY}, 'bridge witness incomplete')
    committed = status(witness['committed'])
    provider = status(witness['provider_after'])
    fresh = status(witness['fresh_status'])
    grid = witness['owner_grid']
    for sample in (grid['bridge_pty'], grid['client'], provider['provider']):
        need(grid_sample(sample)['size'] == committed['committed']['size'], 'bridge grids did not converge')
    exact_target = target(committed['terminal']['target'])
    need(exact_target['runtime_id'] == runtime['runtime_id'] and exact_target['pid'] == runtime['pane_pid']
         and provider['terminal'] == committed['terminal'] and provider['epoch'] == committed['epoch']
         and fresh['terminal']['target'] == exact_target and fresh['committed'] == committed['committed'],
         'probe runtime or authority changed')
    views = [exact(witness[key], 'view_id pid started rss_kib') for key in ('owner', 'victim', 'replacement')]
    need(len({view['view_id'] for view in views}) == len(views)
         and all(uuid_text(view['view_id']) and positive(view['pid']) and positive(view['rss_kib'])
                 and PS_START.fullmatch(view['started']) for view in views), 'bridge attachment scope incomplete')
    retired = views[1]
    cleanup = full_cleanup(Trace(trace_path).read(), retired['view_id'], retired['pid'], anchor)
    need(cleanup is not None, 'bridge reader exit and child reap evidence missing')
    flood = workload(lab.state, 'bridge')
    when = cleanup['times_ns']['full']
    need(flood['started_ns'] <= when <= flood['finished_ns']
         and mono(witness['overflow_returned_at_us'], anchor) >= when, 'bridge Full occurred outside flood')
    for key in ('heartbeat_before', 'heartbeat_after'):
        exact(witness[key], 'pid instance sequence at_ns')
    check_heartbeat(witness['heartbeat_before'], witness['heartbeat_after'], when)
    need(mono(progress['first_at_us'], anchor) <= when < mono(progress['last_at_us'], anchor),
         'healthy bridge output did not advance across Full')
    need(tmux_target(lab, exact_target) == committed['committed']['size'], 'bridge tmux target did not converge')
    return {'outcome': 'passed', 'workload': flood, 'probe': witness, 'retirement': cleanup,
            'input': ledger(lab.state, BRIDGE_INPUT)}


def signed_app(bundle, state, role):
    bundle = bundle.resolve(strict=True)
    metadata = plistlib.loads((bundle / 'Contents/Info.plist').read_bytes())
    name = metadata['CFBundleExecutable']
    bundle_id = metadata['CFBundleIdentifier']
    need(isinstance(name, str) and re.fullmatch(r'[A-Za-z0-9_. -]{1,64}', name)
         and isinstance(bundle_id, str) and re.fullmatch(r'[A-Za-z0-9.-]{1,128}', bundle_id),
         'invalid native application bundle')
    binary = bundle / 'Contents/MacOS' / name
    need(binary.is_file() and not binary.is_symlink(), 'app executable unavailable')
    run_command('/usr/bin/codesign', '--verify', '--strict', bundle)
    signature = run_command('/usr/bin/codesign', '-d', '--verbose=4', bundle).stderr
    code_hash = re.search(r'^CDHash=([0-9a-f]{40,64})$', signature, re.M)
    need(code_hash is not None, 'native build signature identity unavailable')
    unsigned = state / f'unsigned-{role}'
    shutil.copyfile(binary, unsigned)
    run_command('/usr/bin/codesign', '--remove-signature', unsigned)
    return binary, {'bundle_id': bundle_id, 'binary_sha256': digest(binary),
                    'code_hash': code_hash[1], 'unsigned_sha256': digest(unsigned)}


def stop_app(child):
    if child.poll() is None:
        child.terminate()
        try:
            child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait(timeout=5)


def start_app(stack, lab, binary, role):
    path = lab.state / f'pressure-{role}.log'
    environment = dict(os.environ, RS_STATE_DIR=str(lab.state), RS_TMUX_BIN=lab.tmux, RS_W1_RESIZE_TRACE='1')
    with private_writer(path) as log:
        child = subprocess.Popen([str(binary)], stdin=subprocess.DEVNULL,
                                 stdout=subprocess.DEVNULL, stderr=log, env=environment)
    stack.callback(stop_app, child)
    identity = live_process(child.pid)
    return child, identity, Trace(path)


def publish_operator(path, record):
    write_json(path, record)
    print(f'Operator record available at {path}', flush=True)


def raw_grid(trace, expected, view=None, renderer=None):
    return [item for item in trace.grids
            if item['event']['status']['terminal']['target'] == expected
            and (view is None or item['event']['status']['terminal']['view_id'] == view)
            and (renderer is None or item['renderer']['renderer_id'] == renderer)]


def committed_key(record):
    return record['terminal']['target'], record['terminal']['agent_epoch'], record['epoch'], record['committed']


def native_preflight(lab, path, anchor, run_id, expected, owner, victim, opened):
    pause = exact(capture_json(path), 'run_id paused_at_ns owner_view victim_view owner_renderer victim_renderer '
                  'inspector continue_enabled step_enabled no_pending_boundary')
    need(pause['run_id'] == run_id and pause['inspector'] == 'immediate_pause_requested'
         and all(pause[key] is True for key in ('continue_enabled', 'step_enabled', 'no_pending_boundary'))
         and positive(pause['paused_at_ns']) and opened < pause['paused_at_ns'] <= ns(), 'victim pause not witnessed')
    for suffix in ('view', 'renderer'):
        need(uuid_text(pause[f'owner_{suffix}']) and uuid_text(pause[f'victim_{suffix}'])
             and pause[f'owner_{suffix}'] != pause[f'victim_{suffix}'], 'pre-pressure view scopes must be distinct')
    owner_grid = raw_grid(owner.read(), expected, pause['owner_view'], pause['owner_renderer'])
    victim_grid = raw_grid(victim.read(), expected, pause['victim_view'], pause['victim_renderer'])
    for right in reversed(victim_grid):
        right_status = right['event']['status']
        for left in reversed(owner_grid):
            left_status = left['event']['status']
            if (committed_key(left_status) == committed_key(right_status)
                    and left_status['epoch'] is not None
                    and left_status['committed']['intent']['epoch'] == left_status['epoch']
                    and opened < mono(left['event']['client']['sampled_at_us'], anchor) < pause['paused_at_ns']
                    and opened < mono(right['event']['client']['sampled_at_us'], anchor) < pause['paused_at_ns']):
                need(tmux_target(lab, expected) == left_status['committed']['size'], 'pre-pressure tmux grid changed')
                bindings = {}
                for role, trace in (('owner', owner), ('victim', victim)):
                    before = trace.pressure.get(pause[f'{role}_view'], {})
                    need(set(before) == {'attached'}, 'view already retired before intended pressure')
                    bindings[role] = live_process(before['attached']['pid'])
                    tmux_client(lab, expected, bindings[role]['pid'])
                return pause, left, right, bindings
    raise ProofError('missing same committed pre-pressure observations from both actual renderers')


def observe_native_pressure(lab, expected, pause, pre_grid, bindings, owner, victim, anchor):
    trigger = ns()
    (lab.state / 'pressure-native.trigger').touch(mode=0o600, exist_ok=False)
    before = heartbeat(lab.state, expected['pid'])
    samples = [before]
    first_revision = pre_grid['event']['status']['committed']['revision']
    healthy_epoch = pre_grid['event']['status']['epoch']
    latest = None

    def collect():
        nonlocal latest
        owner.read()
        victim.read()
        current = heartbeat(lab.state, expected['pid'])
        if current['sequence'] != samples[-1]['sequence']:
            samples.append(current)
        need(len(samples) <= 800, 'native fixture exceeded pressure window')
        need(process(bindings['owner']['pid']) is not None, 'healthy attachment stopped during pressure')
        post_grids = raw_grid(owner, expected, pause['owner_view'], pause['owner_renderer'])
        for item in reversed(post_grids):
            observation = item['event']['status']
            claimed = observation['committed']
            bridge_at = mono(item['event']['bridge_pty']['sampled_at_us'], anchor)
            client_at = mono(item['event']['client']['sampled_at_us'], anchor)
            if (observation['epoch'] != healthy_epoch or claimed['revision'] <= first_revision
                    or claimed['intent']['epoch'] != healthy_epoch or not trigger < bridge_at <= client_at):
                continue
            match = next((sample for sample in owner.provider_samples()
                          if sample['terminal'] == observation['terminal']
                          and committed_key(sample) == committed_key(observation)
                          and sample['provider'] is not None and sample['observation'] == 'observed'
                          and sample['provider']['size'] == claimed['size']
                          and bridge_at <= mono(sample['provider']['sampled_at_us'], anchor)), None)
            if match is not None:
                tmux = tmux_target(lab, expected)
                if tmux == claimed['size']:
                    if latest is None or claimed['revision'] != latest['grid']['event']['status']['committed']['revision']:
                        latest = {'grid': item, 'provider': match, 'tmux_grid': tmux, 'tmux_sampled_ns': ns()}
                    break
        if not (lab.state / 'pressure-native.json').exists():
            return None
        return workload(lab.state, 'native')

    flood = wait(collect, 16, 'native finite raw flood did not finish')
    records = victim.pressure.get(pause['victim_view'], {})
    need('full' in records, 'overflow not reached in actual victim WebView')
    retirement = wait(lambda: full_cleanup(victim.read(), pause['victim_view'], bindings['victim']['pid'], anchor),
                      2, 'native victim cleanup confirmations missing')
    full = retirement['times_ns']['full']
    need(pause['paused_at_ns'] < trigger <= flood['started_ns'] <= full <= flood['finished_ns'],
         'producer Full did not occur during the paused flood')
    need(latest is not None, 'healthy real owner did not converge four grids during pressure')
    grid = latest['grid']['event']
    need(tmux_target(lab, expected) == grid['status']['committed']['size'],
         'healthy owner committed another grid after the observed convergence')
    provider_at = mono(latest['provider']['provider']['sampled_at_us'], anchor)
    need(flood['started_ns'] <= mono(grid['bridge_pty']['sampled_at_us'], anchor)
         <= mono(grid['client']['sampled_at_us'], anchor) <= provider_at
         <= latest['tmux_sampled_ns'] <= flood['finished_ns'],
         'owner four-grid samples did not overlap the actual producer pressure interval')
    need(same_process(bindings['owner'], live_process(bindings['owner']['pid'])),
         'healthy owner attachment changed through pressure')
    need(not same_process(bindings['victim'], process(bindings['victim']['pid'])),
         'retired victim attachment PID still live')
    before_full = next((sample for sample in reversed(samples) if sample['at_ns'] <= full), None)
    after_full = next((sample for sample in samples if sample['at_ns'] > full), None)
    need(before_full is not None and after_full is not None, 'heartbeat window around native Full missing')
    check_heartbeat(before_full, after_full, full)
    return {'workload': flood, 'retirement': retirement, 'owner_convergence': latest,
            'heartbeat_before': before_full, 'heartbeat_after': after_full}


def validate_raw_operator(data, lab, expected, pause, pressure, pre_victim, owner, victim,
                          anchor, resume_gate, replacement_process):
    exact(data, 'resumed_at_ns resumed_controls closure redraw progress input')
    resumed = data['resumed_at_ns']
    full = pressure['retirement']['times_ns']['full']
    flood = pressure['workload']
    need(positive(resumed) and resume_gate <= resumed < full + 120_000_000_000
         and data['resumed_controls'] is True, 'actual Inspector resume not witnessed')
    closure = exact(data['closure'], 'view_id at_ns closed writable data_empty status_null geometry_null reason high_water')
    closed_at = closure['at_ns']
    need(positive(closed_at) and resumed <= closed_at <= ns() and closure['view_id'] == pause['victim_view']
         and closure['closed'] is True and closure['writable'] is False
         and all(closure[key] is True for key in ('data_empty', 'status_null', 'geometry_null'))
         and closure['reason'] == 'overflow' and closure['high_water'] == CAPACITY,
         'production terminal_read typed closure witness missing')
    need(all(any(entry['view_id'] == pause['victim_view'] and entry['stage'] == stage for entry in victim.failures)
             for stage in ('read', 'renderer_detach')), 'native production stream recovery was not observed')
    redraw = exact(data['redraw'], 'view_id renderer_id at_ns marker revision size automatic old_renderer_disposed '
                   'tab_preserved old_work_absent')
    for key in ('view_id', 'renderer_id'):
        need(uuid_text(redraw[key]), 'invalid replacement scope')
    need(redraw['view_id'] not in (pause['owner_view'], pause['victim_view'])
         and redraw['renderer_id'] not in (pause['owner_renderer'], pause['victim_renderer']),
         'replacement reused a retired or healthy identity')
    need(positive(redraw['at_ns']) and redraw['marker'] == MARKERS['native']
         and all(redraw[key] is True for key in ('automatic', 'old_renderer_disposed', 'tab_preserved', 'old_work_absent')),
         'fresh production redraw on the existing tab was not witnessed')
    attached = victim.pressure.get(redraw['view_id'], {}).get('attached')
    need(attached is not None and replacement_process is not None
         and attached['pid'] == replacement_process['pid']
         and min(closed_at, resumed) <= mono(attached['at_us'], anchor) <= redraw['at_ns'],
         'replacement attachment does not follow actual victim closure')
    pressure_status = pressure['owner_convergence']['grid']['event']['status']
    committed = pressure_status['committed']
    need(redraw['revision'] == committed['revision'] and size(redraw['size']) == committed['size'],
         'replacement redraw used a different committed grid')
    replacement_grids = raw_grid(victim, expected, redraw['view_id'], redraw['renderer_id'])
    need(any(item['event']['status']['committed']['revision'] > redraw['revision']
             and mono(item['event']['client']['sampled_at_us'], anchor) >= redraw['at_ns']
             for item in replacement_grids), 'replacement renderer lacks a post-redraw production observation')

    progress = exact(data['progress'], 'before_ns after_ns first_burst last_burst view_id renderer_id')
    need(all(positive(progress[key]) for key in ('before_ns', 'after_ns'))
         and progress['view_id'] == pause['owner_view'] and progress['renderer_id'] == pause['owner_renderer']
         and flood['started_ns'] <= progress['before_ns'] < full < progress['after_ns'] <= flood['finished_ns']
         and type(progress['first_burst']) is int and type(progress['last_burst']) is int
         and 0 <= progress['first_burst'] < progress['last_burst'] < flood['bursts'],
         'actual healthy owner output did not advance across Full')

    inputs = exact(data['input'], 'owner_detached_ns authorized_ns stale_ack_ns stale_input_ns fresh_ns '
                   'old_view_id old_renderer_id ack_epoch ack_revision ack_size ack_result input_result '
                   'successor_epoch grid_unchanged')
    times = [inputs[key] for key in ('owner_detached_ns', 'authorized_ns', 'stale_ack_ns', 'stale_input_ns', 'fresh_ns')]
    need(all(positive(value) for value in times) and all(a < b for a, b in zip(times, times[1:]))
         and redraw['at_ns'] < times[0] and times[-1] <= ns(), 'native input handoff timing is incomplete')
    former = pre_victim['event']['status']
    need(inputs['old_view_id'] == pause['victim_view'] and inputs['old_renderer_id'] == pause['victim_renderer']
         and inputs['ack_epoch'] == former['epoch'] and inputs['ack_revision'] == former['committed']['revision']
         and size(inputs['ack_size']) == former['committed']['size'] and inputs['ack_result'] == 'View detached'
         and inputs['input_result'] == 'View detached' and inputs['grid_unchanged'] is True,
         'stale native ACK or input rejection was not witnessed')
    successor = epoch(inputs['successor_epoch'])
    need(successor['agent'] == pressure_status['epoch']['agent'] and successor != pressure_status['epoch'],
         'fresh input lacked a successor ownership epoch')
    owner_retirement = owner.pressure.get(pause['owner_view'], {})
    detach = owner_retirement.get('detached')
    need(detach is not None and 'cleanup_complete' in owner_retirement
         and times[0] <= mono(detach['at_us'], anchor) <= times[1],
         'healthy owner did not detach before successor authority')
    need(successor_provider_observed(victim, expected, redraw['view_id'], successor, anchor, times[0], times[-1]),
         'successor epoch was not observed in production status')
    need(tmux_target(lab, expected) == replacement_grids[-1]['event']['status']['committed']['size'],
         'stale commands changed the replacement grid')
    return {'inspector': data, 'replacement_client': replacement_process,
            'replacement_grid': replacement_grids[-1], 'input_delivery': ledger(lab.state, BRIDGE_INPUT + NATIVE_INPUT)}


def managed_executable(lab, runtime_id, profile, tool):
    database = lab.catalog / 'catalog.sqlite3'
    with closing(sqlite3.connect(database.as_uri() + '?mode=ro', uri=True)) as connection:
        row = connection.execute(
            "SELECT o.profile, a.policy FROM create_operations o JOIN runtime_attempts a "
            "ON o.runtime_id = a.id WHERE a.id = ? AND o.stage = 'finished'", (runtime_id,),
        ).fetchone()
    need(row is not None and json.loads(row[0]) == profile, 'installed provider profile did not match launch')
    policy = json.loads(row[1])
    installed = shutil.which(tool)
    need(installed is not None, 'installed provider executable unavailable')
    binary = Path(policy['executable'])
    identity = binary.stat()
    expected = policy['executable_identity']
    need(binary.resolve() == Path(installed).resolve() and expected == [identity.st_dev, identity.st_ino], 'installed provider executable identity changed')
    return binary


def validate_providers(data, lab, anchor, owner, victim, after, agent_epoch):
    exact(data, 'claude_code codex', 'both installed provider flows are required')
    inventory = {item['runtime_id']: item for item in lab.call('list')['runtimes']}
    results = {}
    for profile, tool in (('claude_code', 'claude'), ('codex', 'codex')):
        record = exact(data[profile], 'target paused_ns startup_ns edit_ns resumed_ns owner_view victim_view '
                       'owner_renderer victim_renderer inspector continued prompts_submitted '
                       'trust_auth_unchanged startup_visible unsent_edit_visible')
        scoped = target(record['target'])
        runtime = inventory.get(scoped['runtime_id'])
        need(runtime is not None and runtime['pane_pid'] == scoped['pid'] and not runtime['exited']
             and runtime['origin'] == 'managed', 'installed managed provider runtime was not observed')
        tmux_target(lab, scoped)
        marks = [record[key] for key in ('paused_ns', 'startup_ns', 'edit_ns', 'resumed_ns')]
        need(all(positive(value) for value in marks) and all(a < b for a, b in zip(marks, marks[1:]))
             and after <= marks[0] and marks[-1] <= ns() and type(record['prompts_submitted']) is int
             and record['prompts_submitted'] == 0 and record['inspector'] == 'immediate_pause_requested'
             and all(record[key] is True for key in
                     ('continued', 'trust_auth_unchanged', 'startup_visible', 'unsent_edit_visible')),
             'installed provider startup and unsent editing while paused remain owed')
        need(record['owner_view'] != record['victim_view'] and record['owner_renderer'] != record['victim_renderer'],
             'installed provider requires distinct observed renderers')
        for role, trace in (('owner', owner), ('victim', victim)):
            grids = raw_grid(trace, scoped, record[f'{role}_view'], record[f'{role}_renderer'])
            need(any(item['event']['status']['terminal']['agent_epoch'] == agent_epoch
                     and after <= mono(item['event']['client']['sampled_at_us'], anchor) <= marks[0]
                     for item in grids), 'installed provider renderer scope not independently observed')
        binary = managed_executable(lab, scoped['runtime_id'], profile, tool)
        version_text = output(binary, '--version')
        version = re.search(r'\b\d+\.\d+\.\d+(?:[-+][A-Za-z0-9.\-]+)?\b', version_text)
        need(version is not None, 'installed provider version unavailable')
        producer_events = victim.pressure.get(record['victim_view'], {})
        overflow = 'overflow not reached'
        if 'full' in producer_events:
            when = mono(producer_events['full']['at_us'], anchor)
            need(marks[0] <= when <= marks[-1], 'provider Full fell outside its paused interval')
            overflow = 'full'
        results[profile] = {'operator': record, 'version': version[0],
                            'executable_sha256': digest(Path(binary)), 'provider': live_process(scoped['pid']),
                            'observer_outcome': overflow}
    need(results['claude_code']['operator']['target'] != results['codex']['operator']['target'],
         'provider evidence reused one runtime')
    return results


def native_leg(lab, options, report, expected, anchor, original_process):
    state = lab.state
    owner_bin, owner_build = signed_app(options.owner_app, state, 'owner')
    victim_bin, victim_build = signed_app(options.victim_app, state, 'victim')
    baseline_bin = ROOT / 'target/debug/remote-sessions-client'
    unsigned = state / 'unsigned-source-build'
    shutil.copyfile(baseline_bin, unsigned)
    run_command('/usr/bin/codesign', '--remove-signature', unsigned)
    need(owner_build['bundle_id'] != victim_build['bundle_id']
         and owner_build['unsigned_sha256'] == victim_build['unsigned_sha256'] == digest(unsigned),
         'dual apps must be signed copies of the same local debug binary with distinct bundle IDs')
    builds = {'owner': owner_build, 'victim': victim_build}
    report['app_builds'] = builds
    pause_path = state / 'pressure-operator-pause.json'
    ready_path = state / 'pressure-operator-ready.json'
    gate_path = state / 'pressure-operator-resume.json'
    run_id = report['run_id']
    source = {key: report['build'][key] for key in ('source', 'tree')}
    with ExitStack() as apps:
        owner_child, owner_process, owner = start_app(apps, lab, owner_bin, 'owner')
        publish_operator(state / 'pressure-owner-ready.json', {
            'run_id': run_id, **source, 'builds': builds, 'lab_id': lab.id, 'state_dir': str(state),
            'agent_pid': report['agent']['pid'], 'owner_app': owner_process,
            'target': expected, 'clock': anchor,
        })
        print('Open the raw runtime in the owner app and resize once to bind its renderer.', flush=True)

        def owner_opened():
            need(owner_child.poll() is None, 'owner app exited during setup')
            return raw_grid(owner.read(), expected)

        wait(owner_opened, NATIVE_OWNER_SETUP_SECONDS, 'real healthy owner view did not open')
        victim_child, victim_process, victim = start_app(apps, lab, victim_bin, 'victim')
        opened = ns()
        publish_operator(ready_path, {
            'run_id': run_id, **source, 'builds': builds, 'target': expected, 'clock': anchor,
            'lab_id': lab.id, 'agent_pid': report['agent']['pid'], 'fixture': original_process,
            'owner_app': owner_process, 'victim_app': victim_process,
            'owner_log': str(owner.path), 'victim_log': str(victim.path),
            'pause_record': str(pause_path), 'resume_record': str(gate_path),
            'final_record': str(options.native_evidence),
        })
        print('Open the same raw runtime as spectator, resize from the owner, then pause only the victim JavaScript. '
              'Submit the pause record to trigger the raw flood and resize the owner during the flood.', flush=True)

        def pause_ready():
            need(owner_child.poll() is None and victim_child.poll() is None, 'actual WebView app exited')
            return pause_path.exists()

        wait(pause_ready, NATIVE_INSPECTOR_SETUP_SECONDS, 'Inspector pause record was not supplied')
        pause, pre_owner, pre_victim, clients = native_preflight(
            lab, pause_path, anchor, run_id, expected, owner, victim, opened)
        need(same_process(original_process, live_process(expected['pid'])), 'native fixture process changed')
        pressure = observe_native_pressure(lab, expected, pause, pre_owner, clients, owner, victim, anchor)
        need(same_process(original_process, live_process(expected['pid']))
             and same_process(owner_process, live_process(owner_child.pid))
             and same_process(victim_process, live_process(victim_child.pid)),
             'native runtime or app identities changed during pressure')
        report['native'] = {
            'pause': pause, 'pre_owner': pre_owner, 'pre_victim': pre_victim, 'clients': clients,
            'app_processes': {'owner': owner_process, 'victim': victim_process}, **pressure,
        }
        resume_gate = ns()
        publish_operator(gate_path, {
            'run_id': run_id, 'resume_after_ns': resume_gate, 'retirement': pressure['retirement'],
            'healthy_owner_convergence': pressure['owner_convergence'],
            'held_marker': MARKERS['native'], 'target': expected,
        })
        print('Resume the victim, inspect the production typed closure, and capture the redraw, '
              'handoff and both provider flows. Submit the final private record when complete.', flush=True)

        replacements = {}

        def evidence_ready():
            need(owner_child.poll() is None and victim_child.poll() is None, 'actual WebView app exited before evidence')
            owner.read()
            victim.read()
            for view, events in victim.pressure.items():
                attached = events.get('attached')
                if attached and view not in replacements and mono(attached['at_us'], anchor) >= resume_gate:
                    sample = process(attached['pid'])
                    if sample is not None and sample['rss_kib'] > 0:
                        replacements[view] = sample
            return read_if_present(options.native_evidence)

        evidence = wait(evidence_ready, NATIVE_FINAL_RECORD_SECONDS, 'Root native evidence was not supplied')
        exact(evidence, 'run_id source tree builds pause raw providers')
        need(evidence['run_id'] == run_id and all(evidence[key] == source[key] for key in source)
             and evidence['builds'] == builds and evidence['pause'] == pause,
             'native record belongs to a different run, source, or actual app build')
        redraw_id = evidence['raw']['redraw']['view_id']
        native = validate_raw_operator(evidence['raw'], lab, expected, pause, pressure, pre_victim,
                                       owner, victim, anchor, resume_gate, replacements.get(redraw_id))
        report['native']['raw_recovery'] = native
        agent = pre_owner['event']['status']['terminal']['agent_epoch']
        providers = validate_providers(evidence['providers'], lab, anchor, owner, victim,
                                       evidence['raw']['input']['fresh_ns'], agent)
        need(all(data['operator']['target'] != expected for data in providers.values()),
             'provider evidence reused the raw fixture')
        report['native']['providers'] = providers
        report['native']['outcome'] = 'passed'


NATIVE_INSTRUCTIONS = """\
Use two signed copies of the same local debug executable with distinct bundle
identifiers. The runner launches the owner first. In both apps select the
printed raw runtime. Resize the owner once before opening the victim and once
after the victim attaches. Use the private scoped GRID and PRESSURE records to
bind both view and renderer IDs. Open the victim Inspector from the HTML
header to avoid sending mouse input through xterm. If using an operator fetch
wrapper, capture the original fetch in a lexical const and guard installation
so rerunning the snippet reuses the wrapper. Verify the ready result confirms
the wrapper and the exact victim view. If Console output looks stale, switch
to Sources and back to Console to refresh it before checking that result.
Require both scoped GRID records to acknowledge the same committed revision
and grid with no pending boundary before triggering pressure. In the victim
Inspector choose Sources > Pause script execution. Confirm Immediate Pause
Requested and enabled Continue and Step. Keep the owner running. Write
pressure-operator-pause.json with these exact fields:

  run_id paused_at_ns owner_view victim_view owner_renderer victim_renderer
  inspector="immediate_pause_requested" continue_enabled=true
  step_enabled=true no_pending_boundary=true

The harness starts the finite flood on receipt. Resize the actual owner during
the flood, then stop resize proposals. Observe actual owner output ordinals
RSPROGRESSnnnn across Full (use a non-pausing Inspector logpoint in the owner
write callback to record these scalars from the xterm buffer after parsing).
Convert logpoint wall-clock microseconds as mono_ns + (wall_us - clock.wall_us)
* 1000 using the ready record's clock anchor. Wait for pressure-operator-resume.json. The final screen marker
is now held and the paused victim's Rust reader/client are gone.

Continue the victim from Sources. Use a breakpoint at the typed-closure branch
in TerminalView to inspect the retiring view and the parsed chunk. Continue
through renderer disposal and automatic reconnect. Read RSNATIVEKEPT from the
replacement xterm before any test input or further resize. Leave the tab open.
Then make a later healthy owner geometry change to bind the replacement view
and renderer to a production GRID observation. Close the healthy raw tab
normally and wait until the replacement owns the runtime. In the victim,
invoke the existing terminal_acknowledge_grid with the old view ID, renderer
ID, epoch, revision and size, and terminal_input with the old view ID,
RS-NATIVE-STALE followed by CR and paste=false. Record the actual errors.
Confirm the grid is unchanged. Type RS-NATIVE-FRESH and press Enter once in
the replacement xterm. Do not retype or paste the fresh marker.

For each installed provider, use normal managed create and normal trust/auth
settings. Bind each real view/renderer with a grid change. Observe the actual
startup and unsent editing in the owner during an Inspector pause of that
provider's victim view. Resume the victim without sending any model prompt.
Keep these processes running for the final live identity sample.

Write the final --native-evidence JSON only after the observations. Nested
records use these exact fields (bare names below are JSON keys):

  run_id source tree builds pause raw providers
  builds: copy the exact owner/victim identities from the ready record
  pause: copy the exact pause record
  raw: resumed_at_ns resumed_controls closure redraw progress input
  raw.closure: view_id at_ns closed writable data_empty status_null
               geometry_null reason="overflow" high_water=16
  raw.redraw: view_id renderer_id at_ns marker="RSNATIVEKEPT" revision
              size automatic old_renderer_disposed tab_preserved old_work_absent
  raw.progress: before_ns after_ns first_burst last_burst view_id renderer_id
  raw.input: owner_detached_ns authorized_ns stale_ack_ns stale_input_ns fresh_ns
             old_view_id old_renderer_id ack_epoch ack_revision ack_size
             ack_result input_result successor_epoch grid_unchanged
  providers: claude_code codex
  each provider: target paused_ns startup_ns edit_ns resumed_ns owner_view
                 victim_view owner_renderer victim_renderer prompts_submitted=0
                 inspector="immediate_pause_requested" continued
                 trust_auth_unchanged startup_visible unsent_edit_visible
  target: runtime_id generation session window pane pid process_start
  size, ack_size: cols rows
  ack_epoch, successor_epoch: agent owner

Record monotonic times in nanoseconds. The ready record pairs the Mac monotonic
clock with the wall-clock timestamps in the native debug records. Record
observation booleans only after the actual UI/Inspector events. Do not put
raw terminal content, source snippets, private input bytes, screenshots,
environment values or broad logs in the submitted records.
"""


def prove(options, report):
    sys.path.insert(0, str(ROOT / 'scripts'))
    from runtime_lab import Lab

    probe = ROOT / 'target/debug/examples/probe'
    need(probe.is_file(), 'debug bridge probe is unavailable')
    anchor = sample_clock()
    lab = Lab(w1=True, pressure=True)
    state = lab.state
    observed_processes = []
    report['lab_id'] = lab.id
    report['clock'] = anchor
    try:
        with lab:
            report['build'] = versions(probe, lab.service_host, lab.tmux)
            created = lab.call('create', '--cwd', state, '--', sys.executable,
                               Path(__file__).resolve(), '--fixture', state)
            runtime = created['runtime']
            pid = runtime['pane_pid']
            need(positive(pid) and uuid_text(runtime['runtime_id']), 'raw runtime did not start')
            provider = live_process(pid)
            observed_processes.append(provider)
            report['agent'] = live_process(lab.call('health')['agent_pid'])
            observed_processes.append(report['agent'])
            wait(lambda: (state / 'pressure-startup.json').exists(), 6, 'startup flood did not finish')
            report['fixture'] = provider
            report['startup'] = workload(state, 'startup')
            report['bridge'] = bridge_leg(lab, probe, runtime, report, anchor)
            expected = report['bridge']['probe']['committed']['terminal']['target']
            report['tmux_server'] = server = tmux_server(lab)
            observed_processes.append(server['process'])
            need(server['generation'] == expected['generation'], 'tmux generation changed')
            need(same_process(provider, live_process(pid)), 'raw provider process was replaced')
            if options.native_evidence:
                native_leg(lab, options, report, expected, anchor, provider)
                observed_processes.extend(data['provider'] for data in report['native']['providers'].values())
            engine = tmux_server(lab)
            need(same_process(server['process'], engine['process']) and
                 (server['started'], server['generation']) == (engine['started'], engine['generation']),
                 'dedicated tmux changed during workload')
            finished_clock = sample_clock()
            need(abs(mono(finished_clock['wall_us'], anchor) - finished_clock['mono_ns']) <= 50_000_000,
                 'wall and monotonic clocks drifted during evidence collection')
            need(versions(probe, lab.service_host, lab.tmux) == report['build'],
                 'source or binaries changed during workload')
    finally:
        if lab.cleanup_confirmed:
            wait(lambda: all(not same_process(item, process(item['pid'])) for item in observed_processes),
                 5, 'owned fixture or agent process survived lab cleanup')
            report['cleanup'] = 'confirmed'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--report', type=Path, help='New private JSON report destination')
    parser.add_argument('--native-evidence', type=Path, help='New same-run Root operator JSON record')
    parser.add_argument('--owner-app', type=Path, help='Signed debug owner application bundle')
    parser.add_argument('--victim-app', type=Path, help='Signed debug victim application bundle')
    parser.add_argument('--describe-native', action='store_true', help='Print the native operator sequence and record schema')
    parser.add_argument('--fixture', type=Path, help=argparse.SUPPRESS)
    parser.add_argument('--test-record-publication', action='store_true',
                        help='Run the evidence publication regression without starting the lab')
    parser.add_argument('--test-trace-retention', action='store_true',
                        help='Run the diagnostic retention regression without starting the lab')
    options = parser.parse_args()
    if options.test_trace_retention:
        test_trace_retention()
        print('Trace retention regression passed.')
        return 0
    if options.test_record_publication:
        test_record_publication()
        print('Evidence publication regression passed.')
        return 0
    if options.fixture:
        fixture(options.fixture)
        return 0
    if options.describe_native:
        print(NATIVE_INSTRUCTIONS)
        return 0
    if not options.report:
        parser.error('--report is required')
    if bool(options.owner_app) != bool(options.native_evidence) or bool(options.victim_app) != bool(options.native_evidence):
        parser.error('--native-evidence requires both --owner-app and --victim-app')
    if options.native_evidence:
        options.native_evidence = options.native_evidence.absolute()
        if os.path.lexists(options.native_evidence) or not options.native_evidence.parent.is_dir():
            parser.error('--native-evidence must be a new file in an existing directory')
        if options.report.absolute() == options.native_evidence:
            parser.error('--report and --native-evidence must refer to separate files')
    try:
        output_file = private_writer(options.report)
    except OSError:
        parser.error('--report must be a new private output file')
    report = {'format': 'w1-terminal-pressure-1', 'run_id': str(uuid.uuid4()),
              'started_ns': ns(), 'bridge': {'outcome': 'owed'}, 'native': {'outcome': 'owed'}}
    if options.native_evidence:
        report['native_operator_budget_seconds'] = {
            'owner_setup': NATIVE_OWNER_SETUP_SECONDS, 'inspector_setup': NATIVE_INSPECTOR_SETUP_SECONDS,
            'final_record': NATIVE_FINAL_RECORD_SECONDS, 'fixture_grace': NATIVE_FIXTURE_GRACE_SECONDS,
        }
    with output_file:
        try:
            need(sys.platform == 'darwin', 'pressure acceptance requires a logged-in Mac')
            prove(options, report)
        except ProofError as error:
            report['failure'] = str(error)
            if str(error) == 'overflow not reached in actual victim WebView':
                report['native']['outcome'] = 'overflow not reached'
        except KeyboardInterrupt:
            report['failure'] = 'operator interrupted run'
        except (OSError, ValueError, TypeError, LookupError, RuntimeError, sqlite3.Error, subprocess.SubprocessError):
            report['failure'] = 'fixture, command, or evidence record was unavailable or malformed'
        report['finished_ns'] = ns()
        report['passed'] = ('failure' not in report and report.get('cleanup') == 'confirmed'
                            and report['bridge']['outcome'] == 'passed'
                            and (options.native_evidence is None or report['native']['outcome'] == 'passed'))
        json.dump(report, output_file, indent=2)
        output_file.write('\n')
    print('Pressure report complete.' if report['passed'] else 'Pressure evidence incomplete.', flush=True)
    return 0 if report['passed'] else 1


if __name__ == '__main__':
    sys.exit(main())
