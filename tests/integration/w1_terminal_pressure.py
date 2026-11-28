#!/usr/bin/env python3
"""Run the installed terminal pressure fixture in its own logged-in Mac lab.

Write each operator JSON record atomically with mode 0600. Times ending in _ns are from
time.monotonic_ns() on this Mac. Keep screenshots and terminal text outside
these records. Use --describe-native to print the operator sequence and schema.
"""
import argparse
from contextlib import ExitStack, closing, nullcontext
import ctypes
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
from tempfile import TemporaryDirectory
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
HOST_PATH = Path('target/debug/host-agent')
PROBE_PATH = Path('target/debug/examples/probe')
APP_PATH = Path('target/debug/bundle/macos/Remote Sessions.app')
LOCKFILES = ('Cargo.lock', 'package-lock.json')
BUILD_MODES = ('bridge', 'installed', 'paste')
BUILD_TIMEOUT = 600
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
STALE_INPUT = {'victim': b'RS-SPECTATOR-STALE\r', 'owner': b'RS-WRITER-STALE\r'}
RETIRED_CALLS = {
    'victim': (('terminal_input', 'input'), ('terminal_acknowledge_grid', 'acknowledge')),
    'owner': (('terminal_input', 'input'), ('terminal_read', 'read')),
}
REJECTION_FIELDS = 'reason view_id rejection_id event_time_us command stage'
INSPECTOR_TICK_NS = 1_000_000
SYNTHETIC_INCARNATION = '00000000-0000-4000-8000-000000000001'
PS_START = re.compile(r'[A-Z][a-z]{2} [A-Z][a-z]{2}\s[ 0-9][0-9] \d\d:\d\d:\d\d \d{4}')


class ProofError(Exception):
    """An evidence failure whose fixed message is safe to publish."""


class ProvenanceHold(ProofError):
    pass


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


def capture_json(path, expected_sha256=None):
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
    if expected_sha256 is not None and hashlib.sha256(data).hexdigest() != expected_sha256:
        raise ProvenanceHold('build record differs from the independently retained producer digest')

    return json.loads(data, object_pairs_hook=unique_evidence_fields)


def unique_evidence_fields(pairs):
    fields = dict(pairs)
    need(len(fields) == len(pairs), 'duplicate evidence key')
    return fields


def write_json(path, record, replace=False):
    pending = path.with_name(path.name + '.tmp')
    data = json.dumps(record, separators=(',', ':')) + '\n'
    need(len(data) <= MAX_RECORD, 'evidence record exceeds publication limit')
    with private_writer(pending) as output:
        output.write(data)
    if replace:
        pending.replace(path)
    else:
        try:
            os.link(pending, path)
        finally:
            pending.unlink()
    return hashlib.sha256(data.encode('ascii')).hexdigest()


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


def run_command(*args, timeout=10, check=True, cwd=None, env=None):
    result = subprocess.run([str(arg) for arg in args], stdin=subprocess.DEVNULL,
                            capture_output=True, text=True, timeout=timeout, cwd=cwd,
                            env=dict(os.environ if env is None else env, LC_ALL='C'),
                            encoding='utf-8', errors='surrogateescape')
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


def versions(build, tmux):
    return {
        'source': build['before']['commit'], 'tree': build['before']['tree'],
        'probe_sha256': build['artifacts']['probe']['sha256'], 'host_sha256': build['artifacts']['host']['sha256'],
        'tmux_sha256': digest(Path(tmux)),
        'tmux': output(tmux, '-V'), 'macos': output('/usr/bin/sw_vers', '-productVersion'),
        'macos_build': output('/usr/bin/sw_vers', '-buildVersion'),
        'machine': output('/usr/bin/uname', '-m'), 'rustc': build['tools']['rustc']['version'],
        'python': f'{sys.version_info.major}.{sys.version_info.minor}.{sys.version_info.micro}',
    }


def sample_clock():
    before = ns()
    wall_ns = time.time_ns()
    after = ns()
    return {'mono_ns': (before + after) // 2, 'wall_us': wall_ns // 1000,
            'wall_ns': wall_ns, 'before_ns': before, 'after_ns': after}


def mono(at_us, anchor):
    need(positive(at_us), 'invalid observation timestamp')
    return anchor['mono_ns'] + (at_us - anchor['wall_us']) * 1000


def wall_interval(wall_ns, width_ns, anchor):
    need(all(positive(anchor.get(field)) for field in ('before_ns', 'after_ns', 'wall_ns')),
         'native clock anchor is incomplete')
    lower = anchor['before_ns'] - anchor['wall_ns']
    upper = anchor['after_ns'] - anchor['wall_ns']
    need(0 <= upper - lower <= INSPECTOR_TICK_NS, 'native clock anchor is imprecise')
    return wall_ns + lower, wall_ns + width_ns + upper


def corroborate_clock(anchor, later):
    lower = min(anchor['before_ns'] - anchor['wall_ns'], later['before_ns'] - later['wall_ns'])
    upper = max(anchor['after_ns'] - anchor['wall_ns'], later['after_ns'] - later['wall_ns'])
    need(0 <= upper - lower <= INSPECTOR_TICK_NS, 'wall and monotonic clocks drifted during invocation')
    return {**anchor, 'before_ns': anchor['wall_ns'] + lower, 'after_ns': anchor['wall_ns'] + upper}


def inspector_interval(ms, anchor):
    return wall_interval((ms - 1) * INSPECTOR_TICK_NS, 2 * INSPECTOR_TICK_NS, anchor)


def detached_rejection(value):
    item = exact(value, REJECTION_FIELDS)
    need(item['reason'] == 'view_detached' and uuid_text(item['view_id'])
         and uuid_text(item['rejection_id']) and positive(item['event_time_us'])
         and any((item['command'], item['stage']) in calls for calls in RETIRED_CALLS.values()),
         'invalid native rejection metadata')
    return item


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

    def __init__(self, path, stream=None):
        self.path = path
        self.stream = stream
        self.offset = 0
        self.pressure = {}
        self.grids = []
        self._providers = {}
        self.failures = []
        self.rejections = []
        self._file_identity = None
        self._size = 0
        self._modified_ns = None
        self._caught_up = None

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

    def _open(self, independent=False):
        if self.stream is not None and not independent:
            return nullcontext(self.stream)
        descriptor = os.open(self.path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        return os.fdopen(descriptor, 'rb')

    def _check_stream(self, stream):
        info = os.fstat(stream.fileno())
        named = self.path.lstat()
        identity = info.st_dev, info.st_ino
        need(stat.S_ISREG(info.st_mode) and os.path.samestat(info, named)
             and (self._file_identity is None or self._file_identity == identity)
             and (info.st_size > self._size or info.st_size == self._size
                  and self._modified_ns in (None, info.st_mtime_ns)), 'native log stream changed')
        self._file_identity = identity
        self._size = info.st_size
        self._modified_ns = info.st_mtime_ns
        return info.st_size

    def boundary(self):
        try:
            with self._open() as stream:
                end = self._check_stream(stream)
                start = max(self.offset, end - MAX_RECORD)
                stream.seek(start)
                suffix = stream.read(end - start)
                need(len(suffix) == end - start, 'native log boundary unavailable')
                newline = suffix.rfind(b'\n')
                need(newline >= 0 or len(suffix) < MAX_RECORD, 'oversized native diagnostic record')
                return start + newline + 1 if newline >= 0 else self.offset
        except (OSError, ValueError) as error:
            raise ProofError('native log boundary unavailable') from error

    def read(self, boundary=None):
        try:
            return self._read_batch(boundary)
        except (OSError, ValueError) as error:
            raise ProofError('native log stream unavailable or malformed') from error

    def _read_batch(self, boundary):
        with self._open() as stream:
            end = self._check_stream(stream)
            if boundary is not None:
                need(self.offset <= boundary <= end, 'native log boundary unavailable')
                end = boundary
            stream.seek(self.offset)
            for start, line in self._lines(stream, end):
                self.offset = stream.tell()
                kind, payload = self._record(line)
                if kind not in (b'PRESSURE', b'GRID', b'PROVIDER', b'FAILURE', b'REJECTION'):
                    continue
                item = self._payload(payload)
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
                elif kind == b'REJECTION':
                    self.rejections.append((detached_rejection(item), start, self.offset))
                else:
                    exact(item, 'view_id stage reason')
                    if item['reason'] == 'stream_closed' and item['stage'] in ('read', 'renderer_detach'):
                        need(uuid_text(item['view_id']), 'invalid stream closure identity')
                        self.failures.append(item)
                need(len(self.pressure) <= 128 and len(self.grids) <= 256
                     and len(self.failures) <= 256 and len(self.rejections) <= 256, 'too many debug observations')
            self._check_stream(stream)
        return self

    @staticmethod
    def _lines(stream, end):
        batch_start = stream.tell()
        for _ in range(8192):
            start = stream.tell()
            if start >= end or start - batch_start >= 4 * 1024 * 1024:
                break
            line = stream.readline(min(MAX_RECORD + 1, end - start))
            if not line or not line.endswith(b'\n'):
                need(len(line) < MAX_RECORD, 'oversized native diagnostic record')
                break
            need(len(line) <= MAX_RECORD, 'oversized native diagnostic record')
            yield start, line

    @staticmethod
    def _record(line):
        if not line.startswith(PREFIX):
            return None, None
        kind, _, payload = line[len(PREFIX):].partition(b' ')
        return kind, payload

    @staticmethod
    def _payload(payload):
        item = json.loads(payload, object_pairs_hook=unique_evidence_fields)
        need(isinstance(item, dict), 'invalid diagnostic payload')
        return item

    def staged_provider(self, observed, sampled_at):
        def matches(sample):
            return (sample['provider'] is not None and sample['provider']['sampled_at_us'] == sampled_at
                    and sample['terminal'] == observed['terminal']
                    and committed_key(sample) == committed_key(observed))

        need(self._caught_up is not None, 'native log capture boundary unavailable')
        boundary, deadline = self._caught_up
        try:
            with self._open() as original:
                need(boundary <= self._check_stream(original), 'native log capture boundary unavailable')
                if self.offset == boundary:
                    cached = next((sample for sample in self.provider_samples() if matches(sample)), None)
                    if cached is not None:
                        return cached

            deadline = min(deadline, time.monotonic() + 2)
            with self._open(independent=True) as stream:
                need(boundary <= self._check_stream(stream), 'native log capture boundary unavailable')
                while stream.tell() < boundary:
                    need(time.monotonic() < deadline, 'native log provider lookup deadline expired')
                    previous = stream.tell()
                    for _, line in self._lines(stream, boundary):
                        kind, payload = self._record(line)
                        if kind == b'PROVIDER':
                            candidate = status(self._payload(payload))
                            if matches(candidate):
                                self._check_stream(stream)
                                need(time.monotonic() < deadline, 'native log provider lookup deadline expired')
                                return candidate
                    need(stream.tell() > previous, 'native log provider lookup made no progress')
                self._check_stream(stream)
                need(time.monotonic() < deadline, 'native log provider lookup deadline expired')
        except (OSError, ValueError) as error:
            raise ProofError('native log stream unavailable or malformed') from error
        return None


def catch_up_role_logs(traces, deadline):
    need(time.monotonic() < deadline, 'native log catch-up deadline expired')
    boundaries = [trace.boundary() for trace in traces]
    while True:
        need(time.monotonic() < deadline, 'native log catch-up deadline expired')
        pending = [(trace, end) for trace, end in zip(traces, boundaries) if trace.offset < end]
        if not pending:
            for trace, end in zip(traces, boundaries):
                trace._caught_up = end, deadline
            return
        for trace, end in pending:
            need(time.monotonic() < deadline, 'native log catch-up deadline expired')
            previous = trace.offset
            trace.read(end)
            need(trace.offset > previous, 'native log boundary made no progress')


def wait_native_record(read_record, traces, budget, message):
    deadline = time.monotonic() + budget

    def poll():
        for trace in traces:
            trace.read()
        record = read_record()
        if record is None:
            return None
        received = ns()
        catch_up_role_logs(traces, deadline)
        return record, received

    return wait(poll, deadline - time.monotonic(), message)


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
    if run_command('/usr/bin/codesign', '-d', unsigned, check=False).returncode == 0:
        run_command('/usr/bin/codesign', '--remove-signature', unsigned)
    return binary, {'bundle_id': bundle_id, 'binary_sha256': digest(binary),
                    'code_hash': code_hash[1], 'unsigned_sha256': digest(unsigned)}


def build_environment():
    env = {key: os.environ[key] for key in ('PATH', 'HOME', 'TMPDIR') if key in os.environ}
    return {**env, 'CI': 'true', 'GIT_OPTIONAL_LOCKS': '0', 'GIT_CONFIG_NOSYSTEM': '1',
            'GIT_CONFIG_GLOBAL': os.devnull, 'CARGO_TARGET_DIR': str(ROOT / 'target'),
            'NPM_CONFIG_USERCONFIG': os.devnull, 'NPM_CONFIG_GLOBALCONFIG': os.devnull}


def source_snapshot(fresh=False, labs=()):
    def git(*argv):
        return run_command('/usr/bin/git', '-c', 'core.fsmonitor=false', '-c', 'core.untrackedCache=false',
                           *argv, cwd=ROOT, env=build_environment(), timeout=60).stdout

    need(Path(git('rev-parse', '--show-toplevel').strip()).resolve() == ROOT, 'unexpected build checkout')
    commit, tree = git('rev-parse', 'HEAD', 'HEAD^{tree}').splitlines()
    need(all(re.fullmatch(r'[0-9a-f]{40}', item) for item in (commit, tree)), 'invalid source identity')
    flags = git('ls-files', '-v', '-z').split('\0')[:-1]
    need(all(entry.startswith('H ') for entry in flags), 'source index contains hidden or unmerged paths')
    status_rows = git('status', '--porcelain=v1', '-z', '--untracked-files=all', '--ignored=matching',
                      '--ignore-submodules=none').split('\0')[:-1]
    generated = ('target/', 'node_modules/', 'apps/client/node_modules/', 'apps/client/dist/',
                 'apps/client/src-tauri/gen/', 'scripts/__pycache__/', 'tests/integration/__pycache__/')
    cache = ROOT / '.cache'
    if not fresh and os.path.lexists(cache):
        receipts = {path for lab in labs for path in (lab.manifest, lab.recovery_path)}
        need(not cache.is_symlink() and cache.is_dir() and set(cache.iterdir()) <= receipts,
             'unexpected build checkout cache inputs')
        generated += ('.cache/',)
    need(all(not fresh and row.startswith('!! ') and row[3:].startswith(generated)
             for row in status_rows), 'dirty or non-fresh build checkout')
    return {'checkout': str(ROOT), 'commit': commit, 'tree': tree, 'staged': [], 'unstaged': [],
            'untracked': [], 'locks': {name: digest(ROOT / name) for name in LOCKFILES}}


def file_identity(path):
    path = Path(path)
    need(path.is_absolute() and path.resolve(strict=True) == path and path.is_file(),
         'artifact path is not an exact regular file')
    before = path.stat()
    sha = digest(path)
    after = path.stat()
    fields = ('st_dev', 'st_ino', 'st_mode', 'st_size', 'st_mtime_ns', 'st_ctime_ns')
    need(all(getattr(before, key) == getattr(after, key) for key in fields), 'artifact changed during hashing')
    return {'path': str(path), 'sha256': sha, 'device': before.st_dev, 'inode': before.st_ino,
            'mode': stat.S_IMODE(before.st_mode), 'size': before.st_size,
            'modified_ns': before.st_mtime_ns, 'changed_ns': before.st_ctime_ns}


def bundle_identity(bundle):
    need(bundle.is_dir() and bundle.resolve(strict=True) == bundle, 'artifact bundle path changed')
    metadata = plistlib.loads((bundle / 'Contents/Info.plist').read_bytes())
    name = metadata['CFBundleExecutable']
    need(isinstance(name, str) and re.fullmatch(r'[A-Za-z0-9_. -]{1,64}', name)
         and name not in ('.', '..'), 'invalid bundle executable')
    binary = file_identity(bundle / 'Contents' / 'MacOS' / name)
    tree_hash = hashlib.sha256()
    for path in sorted(bundle.rglob('*')):
        mode = path.lstat().st_mode
        if stat.S_ISLNK(mode):
            value = str(path.readlink())
        elif stat.S_ISREG(mode):
            value = file_identity(path)['sha256']
        else:
            need(stat.S_ISDIR(mode), 'unexpected bundle file type')
            value = ''
        tree_hash.update(json.dumps([path.relative_to(bundle).as_posix(), mode, value]).encode())
    return {'path': str(bundle), 'tree_sha256': tree_hash.hexdigest(), 'executable': binary}


def build_commands(mode, tools):
    commands = [[tools['make']['path'], 'build-runtime'],
                [tools['cargo']['path'], 'build', '--locked', '-p', 'remote-sessions-desktop-bridge',
                 '--example', 'probe']]
    if mode != 'bridge':
        npm = tools['npm']['path']
        commands.extend([[npm, 'ci'], [npm, 'run', 'tauri', '--', 'build', '--debug', '--bundles',
                                             'app', '--', '--locked']])
    return commands


def build_tools(mode, environment):
    names = ['git', 'make', 'cargo', 'rustc']
    if mode != 'bridge':
        names.extend(('node', 'npm', 'codesign'))
    tools = {}
    for name in names:
        command = (f'/usr/bin/{name}' if name in ('git', 'codesign')
                   else shutil.which(name, path=environment.get('PATH')))
        need(command is not None, 'required controlled build tool is unavailable')
        path = Path(command).absolute()
        args = ('-h',) if name == 'codesign' else ('--version',)
        version = run_command(path, *args, cwd=ROOT, env=environment, check=name != 'codesign')
        tools[name] = {'path': str(path), 'resolved': file_identity(path.resolve(strict=True)),
                       'version': (version.stdout or version.stderr).splitlines()[0]}
    sysroot = run_command(tools['rustc']['path'], '--print', 'sysroot', cwd=ROOT, env=environment).stdout.strip()
    for name in ('cargo', 'rustc'):
        tools[name]['toolchain'] = file_identity((Path(sysroot) / 'bin' / name).resolve(strict=True))
    return tools


def tauri_tool():
    launcher = ROOT / 'node_modules/.bin/tauri'
    package = ROOT / 'node_modules/@tauri-apps/cli'
    executable = launcher.resolve(strict=True)
    need(executable.is_relative_to(package), 'Tauri launcher is outside the locked package')
    version = json.loads((package / 'package.json').read_bytes())['version']
    locked = json.loads((ROOT / 'package-lock.json').read_bytes())['packages']['node_modules/@tauri-apps/cli']
    need(version == locked['version'], 'Tauri launcher does not match the package lock')
    return {'path': str(launcher), 'resolved': file_identity(executable), 'version': version}


def bundle_roles(mode):
    return {'bridge': (), 'installed': ('owner', 'victim'), 'paste': ('owner',)}[mode]


def output_path(path):
    need(path is not None and path.is_absolute(), 'controlled build outputs require absolute paths')
    parent = path.parent.resolve(strict=True)
    meta = parent.stat()
    need(parent.is_dir() and meta.st_uid == os.getuid() and stat.S_IMODE(meta.st_mode) == 0o700
         and not parent.is_relative_to(ROOT), 'controlled build outputs require a private directory outside the checkout')
    checked = parent / path.name
    need(not os.path.lexists(checked), 'controlled build output already exists')
    return checked


def controlled_build(options):
    mode = options.controlled_build
    record_path = output_path(options.build_record)
    need(all(bool(getattr(options, role + '_app')) == (role in bundle_roles(mode))
             for role in ('owner', 'victim')), 'controlled build bundle outputs do not match the mode')
    bundles = {role: output_path(getattr(options, role + '_app')) for role in bundle_roles(mode)}
    need(len(set(bundles.values()) | {record_path}) == len(bundles) + 1, 'controlled build outputs overlap')
    before = source_snapshot(fresh=True)
    need(not any(os.path.lexists(ROOT / path) for path in ('target', 'node_modules', 'apps/client/node_modules',
                                                          'apps/client/dist', 'apps/client/src-tauri/gen')),
         'controlled build requires fresh output paths')
    env = build_environment()
    tools = build_tools(mode, env)
    argv = build_commands(mode, tools)
    tauri = None
    for command in argv:
        run_command(*command, cwd=ROOT, env=env, timeout=BUILD_TIMEOUT)
        if command[1:] == ['ci']:
            tauri = tauri_tool()
    artifacts = {role: file_identity(ROOT / path) for role, path in (('host', HOST_PATH), ('probe', PROBE_PATH))}
    need(all(value['mode'] & stat.S_IXUSR for value in artifacts.values()), 'built executable is not executable')
    if bundles:
        artifacts['unsigned_app'] = bundle_identity(ROOT / APP_PATH)
        with TemporaryDirectory() as scratch:
            temporary = Path(scratch)
            unsigned = temporary / 'source-app'
            shutil.copyfile(artifacts['unsigned_app']['executable']['path'], unsigned)
            if run_command('/usr/bin/codesign', '-d', unsigned, check=False).returncode == 0:
                run_command('/usr/bin/codesign', '--remove-signature', unsigned)
            artifacts['unsigned_app']['unsigned_sha256'] = digest(unsigned)
            for role, bundle in bundles.items():
                bundle.mkdir(mode=0o700)
                shutil.copytree(ROOT / APP_PATH, bundle, dirs_exist_ok=True, symlinks=True)
                info = bundle / 'Contents' / 'Info.plist'
                metadata = plistlib.loads(info.read_bytes())
                metadata['CFBundleIdentifier'] += f'.w1.{mode}.{role}'
                info.write_bytes(plistlib.dumps(metadata))
                signing = ['/usr/bin/codesign', '--force', '--deep', '--sign', '-', str(bundle)]
                run_command(*signing, env=env, timeout=BUILD_TIMEOUT)
                argv.append(signing)
                _, signature = signed_app(bundle, temporary, role)
                need(signature['unsigned_sha256'] == artifacts['unsigned_app']['unsigned_sha256'],
                     'signed copy does not match the built debug app')
                artifacts[role] = {**bundle_identity(bundle), 'signature': signature}
    need(build_tools(mode, env) == tools, 'build tools changed during controlled build')
    if tauri is not None:
        need(tauri_tool() == tauri, 'Tauri launcher changed during controlled build')
        tools['tauri'] = tauri
    after = source_snapshot()
    need(after == before, 'source changed during controlled build')
    record = {'format': 'w1-controlled-build-1', 'mode': mode, 'before': before, 'after': after,
              'tools': tools, 'argv': argv, 'artifacts': artifacts}
    checksum = write_json(record_path, record)
    print(f'BUILD_RECORD_SHA256={checksum}', flush=True)


def verify_artifacts(build, options, mode, labs=()):
    try:
        need(source_snapshot(labs=labs) == build['before'] == build['after'],
             'build source state differs from this checkout')
        artifacts = build['artifacts']
        keys = {'host', 'probe'}
        if mode != 'bridge':
            keys.update(('unsigned_app', *bundle_roles(mode)))
        need(set(artifacts) == keys, 'build artifact roles do not match the mode')
        for role, path in (('host', HOST_PATH), ('probe', PROBE_PATH)):
            need(file_identity(ROOT / path) == artifacts[role], 'built host or probe does not match the receipt')
        if mode != 'bridge':
            unsigned = artifacts['unsigned_app']
            need(bundle_identity(ROOT / APP_PATH) == {key: value for key, value in unsigned.items()
                                                      if key != 'unsigned_sha256'},
                 'unsigned app does not match the receipt')
            with TemporaryDirectory() as scratch:
                identities = []
                for role in bundle_roles(mode):
                    bundle = getattr(options, role + '_app').resolve(strict=True)
                    binary, signature = signed_app(bundle, Path(scratch), role)
                    expected = artifacts[role]
                    need({**bundle_identity(bundle), 'signature': signature} == expected
                         and str(binary) == expected['executable']['path']
                         and signature['unsigned_sha256'] == unsigned['unsigned_sha256'],
                         'signed app copy does not match the receipt')
                    identities.append(signature['bundle_id'])
                need(len(set(identities)) == len(identities), 'signed app roles share a bundle identity')
    except (OSError, ValueError, TypeError, AttributeError, LookupError, ProofError, subprocess.SubprocessError) as error:
        raise ProvenanceHold('source or built artifact could not be verified') from error


def verified_build(options, mode):
    try:
        need(options.build_record is not None and isinstance(options.build_record_sha256, str)
             and re.fullmatch(r'[0-9a-f]{64}', options.build_record_sha256),
             'missing independently retained controlled build digest or record')
        build = exact(capture_json(options.build_record, options.build_record_sha256),
                      'format mode before after tools argv artifacts')
        need(build['format'] == 'w1-controlled-build-1' and build['mode'] == mode,
             'wrong controlled build mode')
        expected = build_commands(mode, build['tools'])
        expected.extend(['/usr/bin/codesign', '--force', '--deep', '--sign', '-', build['artifacts'][role]['path']]
                        for role in bundle_roles(mode))
        need(build['argv'] == expected,
             'wrong controlled build mode or command')
        verify_artifacts(build, options, mode)
        return build
    except ProvenanceHold:
        raise
    except (OSError, ValueError, TypeError, AttributeError, LookupError, ProofError) as error:
        raise ProvenanceHold('controlled build record is missing, malformed or unanchored') from error


def running_artifact(pid, artifact, original=None):
    try:
        first = live_process(pid)
        executable = ctypes.create_string_buffer(4096)
        proc_pidpath = ctypes.CDLL('/usr/lib/libproc.dylib').proc_pidpath
        proc_pidpath.argtypes = (ctypes.c_int, ctypes.c_void_p, ctypes.c_uint32)
        proc_pidpath.restype = ctypes.c_int
        need(proc_pidpath(pid, executable, len(executable)) > 0, 'process executable could not be inspected')
        observed = file_identity(Path(os.fsdecode(executable.value)))
        second = live_process(pid)
        need(observed == artifact and same_process(first, second)
             and (original is None or same_process(original, second)),
             'running executable or process identity differs from the receipt')
        return second
    except (OSError, ValueError, ProofError, subprocess.SubprocessError) as error:
        raise ProvenanceHold('running executable or process identity differs from the receipt') from error


def inspector_windows(pid):
    quartz = ctypes.CDLL('/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics')
    foundation = ctypes.CDLL('/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation')
    pointer = ctypes.c_void_p
    quartz.CGWindowListCopyWindowInfo.argtypes = (ctypes.c_uint32, ctypes.c_uint32)
    quartz.CGWindowListCopyWindowInfo.restype = pointer
    foundation.CFPropertyListCreateData.argtypes = (pointer, pointer, ctypes.c_long, ctypes.c_ulong, pointer)
    foundation.CFPropertyListCreateData.restype = pointer
    foundation.CFDataGetBytePtr.argtypes = (pointer,)
    foundation.CFDataGetBytePtr.restype = pointer
    foundation.CFDataGetLength.argtypes = (pointer,)
    foundation.CFDataGetLength.restype = ctypes.c_long
    foundation.CFRelease.argtypes = (pointer,)
    foundation.CFRelease.restype = None
    array = quartz.CGWindowListCopyWindowInfo(1, 0)
    need(array, 'native window identities are unavailable')
    try:
        data = foundation.CFPropertyListCreateData(None, array, 200, 0, None)
        need(data, 'native window observations could not be decoded')
        try:
            windows = plistlib.loads(ctypes.string_at(foundation.CFDataGetBytePtr(data),
                                                     foundation.CFDataGetLength(data)))
        finally:
            foundation.CFRelease(data)
    finally:
        foundation.CFRelease(array)
    return {item['kCGWindowNumber'] for item in windows if item.get('kCGWindowOwnerPID') == pid}


def lab_host_artifact(lab, build, prior=None):
    try:
        copied = file_identity(lab.service_host)
        host = build['artifacts']['host']
        need(str(lab.host) == host['path'] and lab.startup_argv['agent'][0] == copied['path']
             and copied['sha256'] == host['sha256'] and (prior is None or copied == prior),
             'launchd host copy differs from the built host')
        return copied
    except (OSError, ProofError) as error:
        raise ProvenanceHold('launchd host copy differs from the built host') from error


def launchd_agent_pid(lab):
    sys.path.insert(0, str(ROOT / 'scripts'))
    from runtime_lab import W1Record

    try:
        job = W1Record(lab.recovery_path, fixture=lab.fixture, recovery=True)
        pid = job.loaded_pid('agent')
        need(pid == lab.call('health')['agent_pid'], 'launchd process did not serve the selected lab')
        return pid
    except (OSError, ValueError, LookupError, ProofError, subprocess.SubprocessError) as error:
        raise ProvenanceHold('launchd program or live host could not be verified') from error


def stop_app(child):
    if child.poll() is None:
        child.terminate()
        try:
            child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait(timeout=5)


def start_app(stack, lab, artifact, role):
    path = lab.state / f'pressure-{role}.log'
    environment = dict(os.environ, RS_STATE_DIR=str(lab.state), RS_TMUX_BIN=lab.tmux, RS_W1_RESIZE_TRACE='1')
    with private_writer(path) as log:
        descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        reader = stack.enter_context(os.fdopen(descriptor, 'rb'))
        need(os.path.samestat(os.fstat(log.fileno()), os.fstat(reader.fileno())), 'native log stream changed')
        child = subprocess.Popen([artifact['executable']['path']], stdin=subprocess.DEVNULL,
                                 stdout=subprocess.DEVNULL, stderr=log, env=environment)
    stack.callback(stop_app, child)
    sample = running_artifact(child.pid, artifact['executable'])
    identity = {'pid': sample['pid'], 'started': sample['started'], 'role': role,
                'build': artifact['signature'], 'stderr': str(path)}
    return child, identity, Trace(path, reader)


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


def native_preflight(lab, record, anchor, run_id, expected, owner, victim, opened):
    pause = exact(record, 'run_id paused_at_ns owner_view victim_view owner_renderer victim_renderer '
                  'inspector continue_enabled step_enabled no_pending_boundary')
    need(pause['run_id'] == run_id and pause['inspector'] == 'immediate_pause_requested'
         and all(pause[key] is True for key in ('continue_enabled', 'step_enabled', 'no_pending_boundary'))
         and positive(pause['paused_at_ns']) and opened < pause['paused_at_ns'] <= ns(), 'victim pause not witnessed')
    for suffix in ('view', 'renderer'):
        need(uuid_text(pause[f'owner_{suffix}']) and uuid_text(pause[f'victim_{suffix}'])
             and pause[f'owner_{suffix}'] != pause[f'victim_{suffix}'], 'pre-pressure view scopes must be distinct')
    owner_grid = raw_grid(owner, expected, pause['owner_view'], pause['owner_renderer'])
    victim_grid = raw_grid(victim, expected, pause['victim_view'], pause['victim_renderer'])
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


def validate_replacement(data, expected, pause, pressure, victim, anchor, resume_gate, replacement_process):
    exact(data, 'resumed_at_ns resumed_controls closure redraw progress')
    resumed = data['resumed_at_ns']
    full = pressure['retirement']['times_ns']['full']
    flood = pressure['workload']
    need(positive(resumed) and resume_gate <= resumed < full + 120_000_000_000
         and data['resumed_controls'] is True, 'actual Inspector resume not witnessed')
    closure = exact(data['closure'], 'view_id at_ns closed writable data_empty status_null geometry_null reason high_water')
    closed_at = closure['at_ns']
    need(positive(closed_at) and resumed < closed_at <= ns() and closure['view_id'] == pause['victim_view']
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
         and closed_at < mono(attached['at_us'], anchor) < redraw['at_ns'] <= ns(),
         'replacement attachment does not follow actual victim closure')
    pressure_status = pressure['owner_convergence']['grid']['event']['status']
    committed = pressure_status['committed']
    need(redraw['revision'] == committed['revision'] and size(redraw['size']) == committed['size'],
         'replacement redraw used a different committed grid')
    replacement_grids = [item for item in raw_grid(victim, expected, redraw['view_id'], redraw['renderer_id'])
                         if item['event']['status']['committed']['revision'] > redraw['revision']
                         and item['event']['status']['epoch'] == pressure_status['epoch']
                         and redraw['at_ns'] < mono(item['event']['client']['sampled_at_us'], anchor)]
    need(replacement_grids, 'replacement renderer lacks a post-redraw production observation')

    progress = exact(data['progress'], 'before_ns after_ns first_burst last_burst view_id renderer_id')
    need(all(positive(progress[key]) for key in ('before_ns', 'after_ns'))
         and progress['view_id'] == pause['owner_view'] and progress['renderer_id'] == pause['owner_renderer']
         and flood['started_ns'] <= progress['before_ns'] < full < progress['after_ns'] <= flood['finished_ns']
         and type(progress['first_burst']) is int and type(progress['last_burst']) is int
         and 0 <= progress['first_burst'] < progress['last_burst'] < flood['bursts'],
         'actual healthy owner output did not advance across Full')

    return replacement_grids[-1]


def validate_capture(record, run_id, role, app, trace, expected, anchor, opened, received, scope=None):
    exact(record, 'run_id capture_id inspector_target inspector_window window app captured_ns view_id renderer_id target epoch '
                  'read_sample_us writable grant_sha256 acknowledgement')
    need(record['run_id'] == run_id and record['app'] == app and app['role'] == role
         and trace.path == Path(app['stderr'])
         and uuid_text(record['capture_id']) and uuid_text(record['inspector_target']) and record['window'] == 'main'
         and positive(record['inspector_window'])
         and positive(record['captured_ns']) and opened < record['captured_ns'] <= received
         and uuid_text(record['view_id']) and uuid_text(record['renderer_id']) and record['target'] == expected,
         'Inspector capture came from the wrong app, window, time or terminal view')
    need(scope is None or (record['view_id'], record['renderer_id']) == scope,
         'Inspector capture did not bind the original terminal view')
    view = record['view_id']
    events = trace.pressure.get(view, {})
    attached = events.get('attached')
    detached = events.get('detached')
    at = record['captured_ns']
    grids = raw_grid(trace, expected, view, record['renderer_id'])
    prior = [grid for grid in grids if mono(grid['event']['client']['sampled_at_us'], anchor) <= at]
    need(attached is not None and mono(attached['at_us'], anchor) < at and detached is None and prior,
         'Inspector capture lacks a live role-owned attachment and renderer')
    grid = prior[-1]
    observed = grid['event']['status']
    need(observed['epoch'] is not None and observed['epoch'] == epoch(record['epoch']),
         'Inspector capture disagrees with the live authority interval')
    read_at = mono(record['read_sample_us'], anchor)
    need(mono(attached['at_us'], anchor) < read_at <= at
         and trace.staged_provider(observed, record['read_sample_us']) is not None,
         'capture lacks its role-owned live read status')
    if role == 'victim':
        need(record['writable'] is False and record['grant_sha256'] is None
             and record['acknowledgement'] == grid['renderer'],
             'spectator capture claims a grant or mismatched old grid')
    else:
        digest_value = record['grant_sha256']
        need(record['writable'] is True and record['acknowledgement'] is None
             and isinstance(digest_value, str) and re.fullmatch(r'[0-9a-f]{64}', digest_value)
             and digest_value != hashlib.sha256(SYNTHETIC_INCARNATION.encode()).hexdigest(),
             'writer capture lacks a live opaque input grant')
    return grid


def validate_invocation(record, capture, gate, trace, received, anchor, used_ids, role):
    fields = ('run_id capture_id inspector_target inspector_window window app view_id renderer_id target epoch '
              'read_sample_us grant_sha256')
    exact(record, fields + ' calls clock_ticks')
    need(all(record[key] == capture[key] for key in fields.split())
         and capture['app']['role'] == role and trace.path == Path(capture['app']['stderr'])
         and all(gate[key] == capture[key] for key in
                 ('run_id', 'capture_id', 'inspector_target', 'inspector_window')),
         'retired input did not use the original capture and Inspector page')
    ticks = record['clock_ticks']
    need(isinstance(ticks, list) and len(ticks) == 3 and all(positive(tick) for tick in ticks)
         and ticks[1] == ticks[0] + 1 and ticks[2] == ticks[1] + 1,
         'Inspector wall clock precision was not observed')
    calibration = inspector_interval(ticks[0], anchor)
    need(gate['issued_ns'] < calibration[0], 'Inspector clock calibration predates the invocation gate')
    calls = record['calls']
    expected = RETIRED_CALLS[role]
    need(isinstance(calls, list) and len(calls) == len(expected), 'missing retired native calls')
    role_rejections = [(item, start, end) for item, start, end in trace.rejections
                       if item['view_id'] == capture['view_id'] and start >= gate['log_cursor']]
    need(len(role_rejections) == len(expected), 'unexpected number of role-owned native rejections')
    previous_end = gate['issued_ns']
    previous_offset = gate['log_cursor']
    identities = set()
    witnesses = []
    for call, (command, stage) in zip(calls, expected):
        exact(call, 'command before_ms after_ms rejection')
        need(call['command'] == command and positive(call['before_ms']) and positive(call['after_ms'])
             and ticks[-1] <= call['before_ms'] <= call['after_ms'], 'invalid retired native call')
        before = inspector_interval(call['before_ms'], anchor)
        after = inspector_interval(call['after_ms'], anchor)
        need(previous_end < before[0] <= before[1] < after[0] <= after[1] <= received
             and after[1] - before[0] <= 10_000_000_000, 'ambiguous retired native call window')
        rejection = detached_rejection(call['rejection'])
        identity = rejection['rejection_id']
        event = wall_interval(rejection['event_time_us'] * 1000, 1000, anchor)
        need(identity not in used_ids and identity not in identities
             and rejection['view_id'] == capture['view_id']
             and (rejection['command'], rejection['stage']) == (command, stage)
             and before[1] <= event[0] <= event[1] <= after[0],
             'native rejection falls outside its own retired call')
        matches = [(item, start, end) for item, start, end in trace.rejections
                   if item['rejection_id'] == identity]
        need(len(matches) == 1, 'retired native call lacks a unique role-owned rejection')
        item, start, end = matches[0]
        need(item == rejection and previous_offset <= start < end <= trace.offset,
             'retired native rejection does not match the gated call')
        identities.add(identity)
        witnesses.append({'rejection': item, 'offset': start, 'end_offset': end,
                          'before_ns': list(before), 'after_ns': list(after), 'event_ns': list(event)})
        previous_end = after[1]
        previous_offset = end
    used_ids.update(identities)
    return witnesses


def validate_handoff(handoff, owner, victim, capture, capture_ack_ns, replacement, expected, anchor):
    exact(handoff, 'owner_detached_ns authorized_ns successor_epoch')
    detached, authorized = handoff['owner_detached_ns'], handoff['authorized_ns']
    successor = epoch(handoff['successor_epoch'])
    need(successor['agent'] == capture['epoch']['agent'] and successor != capture['epoch']
         and positive(detached) and positive(authorized) and capture_ack_ns < detached < authorized <= ns(),
         'fresh input lacked a successor ownership epoch after the captured writer')
    events = owner.pressure.get(capture['view_id'], {})
    attached = events.get('attached')
    retired = events.get('detached')
    cleaned = events.get('cleanup_complete')
    need(attached is not None and retired is not None and cleaned is not None
         and attached['pid'] == retired['pid'] == cleaned['pid']
         and detached <= mono(retired['at_us'], anchor) <= mono(cleaned['at_us'], anchor) < authorized,
         'healthy owner did not detach and clean up before successor authority')
    need(successor_provider_observed(victim, expected, replacement, successor, anchor,
                                     mono(cleaned['at_us'], anchor), authorized),
         'successor epoch was not observed in production status')


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


def sample_native_apps(apps, artifacts):
    samples = {}
    for role, (child, original, _) in apps.items():
        need(child.poll() is None and child.pid == original['pid'], 'original native app exited')
        samples[role] = running_artifact(child.pid, artifacts[role]['executable'], original)
        need(child.poll() is None, 'original native app exited during its process sample')
    return samples


def replacement_status(trace, expected, view, authority, committed, anchor, after):
    matching = [sample for sample in trace.read().provider_samples()
                if sample['terminal']['view_id'] == view and sample['terminal']['target'] == expected
                and sample['provider'] is not None
                and mono(sample['provider']['sampled_at_us'], anchor) > after]
    for sample in matching:
        need(sample['epoch'] == authority and sample['committed'] == committed
             and sample['provider']['size'] == committed['size'], 'stale commands changed the replacement authority or grid')
    return bool(matching)


def native_leg(lab, options, report, expected, anchor, original_process, build):
    state = lab.state
    artifacts = build['artifacts']
    builds = {role: artifacts[role]['signature'] for role in ('owner', 'victim')}
    report['app_builds'] = builds
    paths = {stage: state / f'pressure-operator-{stage}.json'
             for stage in ('pause', 'ready', 'resume', 'recovery', 'handoff', 'fresh',
                           'inspectors-open', 'inspectors')}
    for role in ('owner', 'victim'):
        for step in ('capture', 'captured', 'gate', 'invoked'):
            stage = f'{role}-{step}'
            paths[stage] = state / f'pressure-operator-{stage}.json'
    run_id = report['run_id']
    source = {key: report['build'][key] for key in ('source', 'tree')}
    with ExitStack() as stack:
        apps = {'owner': start_app(stack, lab, artifacts['owner'], 'owner')}
        owner_child, owner_app, owner = apps['owner']
        publish_operator(state / 'pressure-owner-ready.json', {
            'run_id': run_id, **source, 'builds': builds, 'lab_id': lab.id, 'state_dir': str(state),
            'agent_pid': report['agent']['pid'], 'owner_app': owner_app,
            'target': expected, 'clock': anchor,
        })
        print('Open the raw runtime in the owner app and resize once to bind its renderer.', flush=True)

        def owner_opened():
            need(owner_child.poll() is None, 'owner app exited during setup')
            return raw_grid(owner.read(), expected)

        wait(owner_opened, NATIVE_OWNER_SETUP_SECONDS, 'real healthy owner view did not open')
        apps['victim'] = start_app(stack, lab, artifacts['victim'], 'victim')
        _, victim_app, victim = apps['victim']
        wait(lambda: inspector_windows(victim_app['pid']), 15, 'victim app window did not open')
        initial_windows = {role: inspector_windows(app[1]['pid']) for role, app in apps.items()}
        opened = ns()
        publish_operator(paths['ready'], {
            'run_id': run_id, **source, 'builds': builds, 'target': expected, 'clock': anchor,
            'lab_id': lab.id, 'agent_pid': report['agent']['pid'], 'fixture': original_process,
            'owner_app': owner_app, 'victim_app': victim_app,
            'initial_windows': {role: sorted(windows) for role, windows in initial_windows.items()},
            'stages': {stage: str(path) for stage, path in paths.items()},
            'final_record': str(options.native_evidence),
        })
        samples = {}
        traces = (owner, victim)

        def operator_record(path, budget, message):
            def read_record():
                need(all(child.poll() is None for child, _, _ in apps.values()), 'original native app exited')
                return read_if_present(path)

            return wait_native_record(read_record, traces, budget, message)

        def stage(name, budget):
            record, received = operator_record(paths[name], budget, f'operator stage {name} was not supplied')
            samples[name] = sample_native_apps(apps, artifacts)
            return record, received

        def acknowledge_capture(role, budget, scope=None):
            record, received = stage(f'{role}-capture', budget)
            _, app, trace = apps[role]
            grid = validate_capture(record, run_id, role, app, trace, expected, anchor, opened, received, scope)
            need(record['inspector_window'] == inspector_ids[role]
                 and record['inspector_window'] in inspector_windows(app['pid']),
                 'capture did not bind the original undocked native Inspector window')
            acknowledged = ns()
            publish_operator(paths[f'{role}-captured'], {
                'run_id': run_id, 'capture_id': record['capture_id'], 'acknowledged_ns': acknowledged,
            })
            return record, grid, acknowledged

        setup_deadline = time.monotonic() + NATIVE_INSPECTOR_SETUP_SECONDS
        print('Open the spectator and both undocked Inspectors. Send the inspectors-open record.', flush=True)
        inspector_request, _ = stage('inspectors-open', NATIVE_INSPECTOR_SETUP_SECONDS)
        need(inspector_request == {'run_id': run_id}, 'Inspector window request belongs to another run')
        inspector_ids = {}
        for role, (_, app, _) in apps.items():
            opened_windows = inspector_windows(app['pid']) - initial_windows[role]
            need(len(opened_windows) == 1, 'expected one new undocked Inspector window per native app')
            inspector_ids[role] = opened_windows.pop()
        publish_operator(paths['inspectors'], {'run_id': run_id, 'windows': inspector_ids})
        spectator, spectator_grid, spectator_ack = acknowledge_capture('victim', setup_deadline - time.monotonic())
        pause_record, _ = stage('pause', setup_deadline - time.monotonic())
        pause, pre_owner, pre_victim, clients = native_preflight(
            lab, pause_record, anchor, run_id, expected, owner, victim, opened)
        need(spectator_ack < pause['paused_at_ns'] and spectator['view_id'] == pause['victim_view']
             and spectator['renderer_id'] == pause['victim_renderer'] and pre_victim == spectator_grid,
             'pause did not follow the acknowledged spectator capture on the same view')
        need(same_process(original_process, live_process(expected['pid'])), 'native fixture process changed')
        pressure = observe_native_pressure(lab, expected, pause, pre_owner, clients, owner, victim, anchor)
        need(same_process(original_process, live_process(expected['pid'])), 'native fixture process changed')
        samples['pressure'] = sample_native_apps(apps, artifacts)
        report['native'] = {
            'pause': pause, 'pre_owner': pre_owner, 'pre_victim': pre_victim, 'clients': clients,
            'app_processes': {'owner': owner_app, 'victim': victim_app}, **pressure,
        }
        resume_gate = ns()
        publish_operator(paths['resume'], {
            'run_id': run_id, 'resume_after_ns': resume_gate, 'retirement': pressure['retirement'],
            'healthy_owner_convergence': pressure['owner_convergence'],
            'held_marker': MARKERS['native'], 'target': expected,
        })

        final_deadline = time.monotonic() + NATIVE_FINAL_RECORD_SECONDS

        def remaining():
            return final_deadline - time.monotonic()

        recovery, _ = stage('recovery', remaining())
        redraw_id = recovery['redraw']['view_id']
        attached = victim.pressure.get(redraw_id, {}).get('attached')
        replacement = live_process(attached['pid']) if attached else None
        replacement_grid = validate_replacement(recovery, expected, pause, pressure, victim, anchor,
                                                resume_gate, replacement)
        tmux_client(lab, expected, replacement['pid'])
        grid_status = replacement_grid['event']['status']
        committed = grid_status['committed']

        used_rejection_ids = set()

        def invoke_retired(role, capture, authority):
            _, _, trace = apps[role]
            need(tmux_target(lab, expected) == committed['size'], 'replacement grid changed before stale input')
            ledger(state, BRIDGE_INPUT)
            before = ns()
            wait(lambda: replacement_status(victim, expected, redraw_id, authority, committed, anchor, before),
                 6, 'replacement authority and grid were not observed before stale input')
            catch_up_role_logs(traces, final_deadline)
            need(capture['inspector_window'] in inspector_windows(capture['app']['pid']),
                 'original Inspector window closed before invocation')
            gate_clock = corroborate_clock(anchor, sample_clock())
            gate = {'run_id': run_id, 'capture_id': capture['capture_id'],
                    'inspector_target': capture['inspector_target'], 'inspector_window': capture['inspector_window'],
                    'log_cursor': trace.offset, 'issued_ns': ns()}
            publish_operator(paths[f'{role}-gate'], gate)
            record, received = stage(f'{role}-invoked', remaining())
            need(capture['inspector_window'] in inspector_windows(capture['app']['pid']),
                 'original Inspector window closed during invocation')
            invocation_clock = corroborate_clock(gate_clock, sample_clock())
            failures = validate_invocation(record, capture, gate, trace, received, invocation_clock,
                                           used_rejection_ids, role)
            finished = failures[-1]['after_ns'][1]
            wait(lambda: replacement_status(victim, expected, redraw_id, authority, committed, anchor,
                                            finished),
                 6, 'replacement authority and grid were not observed after stale input')
            need(tmux_target(lab, expected) == committed['size'], 'stale input changed the runtime grid')
            ledger(state, BRIDGE_INPUT)
            return {'capture': capture, 'gate': gate, 'invocation': record, 'trace': failures}

        detached_spectator = invoke_retired('victim', spectator, grid_status['epoch'])
        writer, writer_grid, writer_ack = acknowledge_capture(
            'owner', remaining(), (pause['owner_view'], pause['owner_renderer']))
        need(writer['view_id'] == pause['owner_view'] and writer['renderer_id'] == pause['owner_renderer']
             and writer['capture_id'] != spectator['capture_id']
             and writer['inspector_target'] != spectator['inspector_target']
             and committed_key(writer_grid['event']['status']) == committed_key(grid_status)
             and detached_spectator['trace'][-1]['after_ns'][1] < writer['captured_ns'],
             'writer capture did not bind the original owner and committed replacement grid')
        handoff, _ = stage('handoff', remaining())
        validate_handoff(handoff, owner, victim, writer, writer_ack, redraw_id, expected, anchor)
        need(not same_process(clients['owner'], process(clients['owner']['pid'])),
             'retired owner attachment PID still live')
        retired_writer = invoke_retired('owner', writer, handoff['successor_epoch'])
        fresh_gate = ns()
        publish_operator(paths['fresh'], {'run_id': run_id, 'view_id': redraw_id,
                                         'epoch': handoff['successor_epoch'], 'issued_ns': fresh_gate})

        evidence, _ = operator_record(options.native_evidence, remaining(), 'final native evidence was not supplied')
        samples['final'] = sample_native_apps(apps, artifacts)
        need(all(record['inspector_window'] in inspector_windows(record['app']['pid'])
                 for record in (spectator, writer)), 'original Inspector window closed before final evidence')
        all_logged_ids = [item['rejection_id'] for trace in traces for item, _, _ in trace.rejections]
        need(len(used_rejection_ids) == 4 and all(all_logged_ids.count(identity) == 1 for identity in used_rejection_ids),
             'native rejection identities were reused across role logs')
        exact(evidence, 'run_id source tree builds pause raw providers')
        need(evidence['run_id'] == run_id and all(evidence[key] == source[key] for key in source)
             and evidence['builds'] == builds and evidence['pause'] == pause,
             'native record belongs to a different run, source, or actual app build')
        raw = exact(evidence['raw'], 'recovery handoff detached_spectator retired_writer fresh')
        need(raw['recovery'] == recovery and raw['handoff'] == handoff
             and raw['detached_spectator'] == detached_spectator['invocation']
             and raw['retired_writer'] == retired_writer['invocation'], 'final record changed a staged observation')
        fresh = exact(raw['fresh'], 'at_ns view_id renderer_id target epoch xterm')
        need(positive(fresh['at_ns']) and fresh_gate < fresh['at_ns'] <= ns()
             and fresh['view_id'] == redraw_id and fresh['renderer_id'] == recovery['redraw']['renderer_id']
             and fresh['target'] == expected and fresh['epoch'] == handoff['successor_epoch']
             and fresh['xterm'] is True and same_process(replacement, live_process(replacement['pid']))
             and same_process(original_process, live_process(expected['pid'])),
             'fresh successor xterm input did not use the authorized replacement')
        report['native']['raw_recovery'] = {
            'inspector': raw, 'replacement_client': replacement, 'replacement_grid': replacement_grid,
            'detached_spectator': detached_spectator, 'retired_writer': retired_writer,
            'app_samples': samples, 'input_delivery': ledger(state, BRIDGE_INPUT + NATIVE_INPUT),
        }
        agent = pre_owner['event']['status']['terminal']['agent_epoch']
        providers = validate_providers(evidence['providers'], lab, anchor, owner, victim, fresh['at_ns'], agent)
        need(all(data['operator']['target'] != expected for data in providers.values()),
             'provider evidence reused the raw fixture')
        report['native']['providers'] = providers
        verify_artifacts(build, options, 'installed', labs=(lab,))
        sample_native_apps(apps, artifacts)
        report['native']['outcome'] = 'passed'


NATIVE_INSTRUCTIONS = """\
Use a fresh disposable checkout of the final clean commit per mode. Use new
absolute output paths in an existing mode-0700 directory outside the checkout.
Run each consumer in its producer's checkout. Capture the producer stdout
directly in a private log and supply its BUILD_RECORD_SHA256 value.

  python3 tests/integration/w1_terminal_pressure.py --controlled-build bridge --build-record "$BRIDGE_RECORD"
  python3 tests/integration/w1_terminal_pressure.py --build-record "$BRIDGE_RECORD" --build-record-sha256 "$BRIDGE_SHA256" --report "$BRIDGE_REPORT"
  python3 tests/integration/w1_terminal_pressure.py --controlled-build installed --build-record "$INSTALLED_RECORD" --owner-app "$OWNER_APP" --victim-app "$VICTIM_APP"
  python3 tests/integration/w1_terminal_pressure.py --build-record "$INSTALLED_RECORD" --build-record-sha256 "$INSTALLED_SHA256" --owner-app "$OWNER_APP" --victim-app "$VICTIM_APP" --native-evidence "$NATIVE_EVIDENCE" --report "$INSTALLED_REPORT"
  python3 tests/integration/w1_terminal_pressure.py --controlled-build paste --build-record "$PASTE_RECORD" --owner-app "$PASTE_APP"
  python3 tests/integration/w1_terminal_pressure.py --paste --build-record "$PASTE_RECORD" --build-record-sha256 "$PASTE_SHA256" --owner-app "$PASTE_APP" --paste-native "$PASTE_NATIVE_EVIDENCE" --report "$PASTE_REPORT"

The paste route stays on HOLD until the prepared workload binds the actual
probe, each live host process and any launchd copy, and the native executable
from the signed WebView bundle before provider observations and before cleanup.
A probe serving as the fixture agent must be observed in both roles. Keep
prior-build negative fixtures and their provenance separate.

For installed pressure, keep both original apps, windows and Inspectors open
through the final sample. Open the same raw runtime in the owner and victim.
Resize from the owner before opening the victim and once after it attaches.
Bind each role's live view, renderer, target and authority to its private
PRESSURE attached and GRID lines. The PRESSURE PID is the tmux client.
The ready record supplies the independently sampled native apps.

Write each staged JSON atomically as mode 0600 at its path in ready.stages.
Use mono_ns + (wall_us - clock.wall_us) * 1000 for wall clock observations.
After ready, open one undocked Inspector per app from the HTML header,
outside xterm. Keep the two windows visible. Send inspectors-open with only
run_id and wait for inspectors, which gives each original window number.
In each original Inspector use a breakpoint immediately after localClient.read
returns the production chunk in TerminalView. Capture a live nonwritable chunk
in the victim before pause. Capture the actual owner's live writable chunk
after victim recovery and before closing the owner tab. Install this operation
once on each original page. Supply clock and evidenceScope in a lexical block
at the breakpoint. evidenceScope has run_id, app copied from ready, renderer_id,
target, epoch, inspector_window from inspectors and acknowledgement. For the
victim use the exact GRID renderer object as acknowledgement. For the owner
use null. Wait for a matching chunk with a status and provider sample.

  (() => {
    const page = globalThis;
    if (page.w1Capture) throw Error("Capture already installed");
    const tauri = page.__TAURI_INTERNALS__;
    const invoke = tauri.invoke.bind(tauri);
    const window = tauri.metadata.currentWindow.label;
    const uuid = () => {
      const bytes = crypto.getRandomValues(new Uint8Array(16));
      bytes[6] = (bytes[6] & 15) | 64;
      bytes[8] = (bytes[8] & 63) | 128;
      const hex = Array.from(bytes, byte => byte.toString(16).padStart(2, "0")).join("");
      return [hex.slice(0, 8), hex.slice(8, 12), hex.slice(12, 16),
              hex.slice(16, 20), hex.slice(20)].join("-");
    };
    const inspector_target = uuid();
    const capture_id = uuid();
    const now = () => clock.mono_ns + (Date.now() * 1000 - clock.wall_us) * 1000;
    const captured_ns = now();
    const writer = evidenceScope.app.role === "owner";
    const read_sample_us = chunk.status?.provider?.sampled_at_us;
    if (chunk.closed || chunk.status?.terminal?.view_id !== view || !read_sample_us ||
        chunk.writable !== writer ||
        Boolean(chunk.paste_incarnation) !== writer) throw Error("Wrong live read");
    const incarnation = writer ? chunk.paste_incarnation : "SPECTATOR_SYNTHETIC_GRANT";
    const viewId = view;
    const { acknowledgement, ...identity } = structuredClone(evidenceScope);
    const bound = { ...identity, view_id: viewId, capture_id, inspector_target, window, read_sample_us };
    const digestGrant = () => {
      if (!writer) return null;
      if (!/^[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}$/.test(incarnation))
        throw Error("Invalid grant shape");
      const block = new Uint8Array(64);
      block.set(new TextEncoder().encode(incarnation));
      block[36] = 128;
      const input = new DataView(block.buffer);
      input.setUint32(60, 36 * 8);
      const words = new Int32Array(64);
      const rotate = (word, bits) => (word >>> bits) | (word << (32 - bits));
      for (let i = 0; i < 16; i++) words[i] = input.getInt32(i * 4);
      for (let i = 16; i < 64; i++) {
        const left = words[i - 15], right = words[i - 2];
        words[i] = words[i - 16] + words[i - 7] +
          (rotate(left, 7) ^ rotate(left, 18) ^ (left >>> 3)) +
          (rotate(right, 17) ^ rotate(right, 19) ^ (right >>> 10));
      }
      const initial = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
                       0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
      const rounds = ("428a2f98 71374491 b5c0fbcf e9b5dba5 3956c25b 59f111f1 923f82a4 ab1c5ed5 " +
        "d807aa98 12835b01 243185be 550c7dc3 72be5d74 80deb1fe 9bdc06a7 c19bf174 " +
        "e49b69c1 efbe4786 0fc19dc6 240ca1cc 2de92c6f 4a7484aa 5cb0a9dc 76f988da " +
        "983e5152 a831c66d b00327c8 bf597fc7 c6e00bf3 d5a79147 06ca6351 14292967 " +
        "27b70a85 2e1b2138 4d2c6dfc 53380d13 650a7354 766a0abb 81c2c92e 92722c85 " +
        "a2bfe8a1 a81a664b c24b8b70 c76c51a3 d192e819 d6990624 f40e3585 106aa070 " +
        "19a4c116 1e376c08 2748774c 34b0bcb5 391c0cb3 4ed8aa4a 5b9cca4f 682e6ff3 " +
        "748f82ee 78a5636f 84c87814 8cc70208 90befffa a4506ceb bef9a3f7 c67178f2")
        .split(" ").map(word => parseInt(word, 16));
      let [a, b, c, d, e, f, g, h] = initial;
      for (let i = 0; i < 64; i++) {
        const first = h + (rotate(e, 6) ^ rotate(e, 11) ^ rotate(e, 25)) +
          ((e & f) ^ (~e & g)) + rounds[i] + words[i];
        const second = (rotate(a, 2) ^ rotate(a, 13) ^ rotate(a, 22)) +
          ((a & b) ^ (a & c) ^ (b & c));
        [a, b, c, d, e, f, g, h] = [(first + second) | 0, a, b, c, (d + first) | 0, e, f, g];
      }
      return [a, b, c, d, e, f, g, h]
        .map((word, i) => ((word + initial[i]) >>> 0).toString(16).padStart(8, "0")).join("");
    };
    const nextTick = previous => {
      const limit = performance.now() + 50;
      for (let i = 0; i < 100000 && performance.now() < limit; i++) {
        const current = Date.now();
        if (current > previous) return current;
      }
      throw Error("Inspector wall clock precision unavailable");
    };
    const rejected = async (command, args) => {
      const before_ms = Date.now();
      nextTick(nextTick(nextTick(before_ms)));
      let rejection = null;
      try { await invoke(command, { ...args, diagnostic: true }); }
      catch (error) {
        const uuidText = /^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/;
        const stages = { terminal_input: "input", terminal_read: "read",
                         terminal_acknowledge_grid: "acknowledge" };
        if (error?.reason === "view_detached" && Object.keys(error).length === 6 &&
            error.view_id === viewId && error.command === command && error.stage === stages[command] &&
            typeof error.rejection_id === "string" && uuidText.test(error.rejection_id) &&
            Number.isSafeInteger(error.event_time_us) && error.event_time_us > 0) {
          rejection = error;
        }
      }
      const after_ms = nextTick(nextTick(nextTick(Date.now())));
      return { command, before_ms, after_ms, rejection };
    };
    let used = false;
    page.w1Capture = Object.freeze({
      capture: () => ({ ...structuredClone(bound), captured_ns, writable: writer,
                       acknowledgement: structuredClone(acknowledgement),
                       grant_sha256: digestGrant() }),
      invoke: async gate => {
        if (used || globalThis !== page || page.__TAURI_INTERNALS__ !== tauri ||
            gate.run_id !== bound.run_id || gate.capture_id !== capture_id ||
            gate.inspector_target !== inspector_target ||
            gate.inspector_window !== bound.inspector_window) throw Error("Wrong capture gate");
        used = true;
        const clock_ticks = [nextTick(Date.now())];
        clock_ticks.push(nextTick(clock_ticks[0]));
        clock_ticks.push(nextTick(clock_ticks[1]));
        const marker = writer ? "RS-WRITER-STALE" : "RS-SPECTATOR-STALE";
        const data = [...new TextEncoder().encode(marker), 13];
        const input = await rejected("terminal_input", { viewId, incarnation, data, paste: false });
        nextTick(nextTick(nextTick(input.after_ms)));
        const followup = writer
          ? await rejected("terminal_read", { viewId })
          : await rejected("terminal_acknowledge_grid", { viewId, acknowledgement });
        return { ...structuredClone(bound), clock_ticks, calls: [input, followup],
                 grant_sha256: digestGrant() };
      }
    });
  })()

While still at the breakpoint, save only the output of w1Capture.capture().
Continue after the corresponding capture acknowledgement. Save only the
output of await w1Capture.invoke(gate) from that original Inspector. Do not
reload, reopen the Inspector or copy the closure to another page. The
synthetic spectator incarnation and real writer grant remain in memory.
Never print or save the raw chunk or either incarnation.

Send victim-capture and wait for victim-captured before pausing only the
victim with Sources > Pause script execution. Confirm Immediate Pause
Requested and enabled Continue and Step. Send pause.

  pause: run_id paused_at_ns owner_view victim_view owner_renderer victim_renderer
         inspector="immediate_pause_requested" continue_enabled=true
         step_enabled=true no_pending_boundary=true

Resize the owner during the triggered finite flood and then stop resizing.
Use a non-pausing logpoint after xterm parses output to observe the owner
RSPROGRESSnnnn across Full. Wait for resume. Continue the victim to a breakpoint
at the typed-closure branch. Record the retiring view and closure before
allowing any replacement attach. Continue through disposal and automatic
reconnect. Observe RSNATIVEKEPT in the replacement xterm before further resize
or test input. Resize from the owner again to produce a scoped replacement
GRID. Send recovery and hold the grid steady until fresh input.

  recovery: resumed_at_ns resumed_controls closure redraw progress
  closure: view_id at_ns closed writable data_empty status_null geometry_null
           reason="overflow" high_water=16
  redraw: view_id renderer_id at_ns marker="RSNATIVEKEPT" revision size
          automatic old_renderer_disposed tab_preserved old_work_absent
  progress: before_ns after_ns first_burst last_burst view_id renderer_id

After victim-gate, invoke once in that original Inspector and send
victim-invoked. Then capture the live owner grant and send owner-capture.
Wait for owner-captured before closing the raw owner tab normally. Leave
the original window and Inspector open. Wait for owner cleanup_complete
and the replacement's writable successor chunk, then send handoff. After
owner-gate invoke once from the retained closure and send owner-invoked.

  capture: run_id capture_id inspector_target inspector_window window app
           captured_ns view_id renderer_id target epoch read_sample_us writable
           grant_sha256 acknowledgement
  invocation: run_id capture_id inspector_target inspector_window window app
              view_id renderer_id target epoch read_sample_us grant_sha256 clock_ticks calls
  clock_ticks: three consecutive observed wall milliseconds
  calls: two ordered entries with command before_ms after_ms rejection
  Keep the clock tick spacing in the closure so the uncertainty bounds for
  the four calls remain distinguishable even when a rejection is immediate.
  rejection: reason view_id rejection_id event_time_us command stage
  handoff: owner_detached_ns authorized_ns successor_epoch

Wait for fresh. Type RS-NATIVE-FRESH and press Enter once in the successor
xterm. For each installed provider use normal managed create and trust/auth.
Bind each view and renderer with a grid change. Observe startup and unsent
editing in the owner during that provider's victim Inspector pause. Resume the
victim. Submit zero model prompts and keep the provider processes live.

Send the final --native-evidence JSON with these exact fields:

  run_id source tree builds pause raw providers
  raw: recovery handoff detached_spectator retired_writer fresh
  detached_spectator: exact victim-invoked record
  retired_writer: exact owner-invoked record
  fresh: at_ns view_id renderer_id target epoch xterm=true
  providers: claude_code codex
  each provider: target paused_ns startup_ns edit_ns resumed_ns owner_view
                 victim_view owner_renderer victim_renderer prompts_submitted=0
                 inspector="immediate_pause_requested" continued
                 trust_auth_unchanged startup_visible unsent_edit_visible
  target: runtime_id generation session window pane pid process_start
  size: cols rows
  epoch: agent owner

Keep raw terminal bytes, raw grants, screenshots, environment values and broad
logs out of the records. Record booleans after observing the corresponding UI.
""".replace('SPECTATOR_SYNTHETIC_GRANT', SYNTHETIC_INCARNATION)


def prove(options, report, build):
    sys.path.insert(0, str(ROOT / 'scripts'))
    from runtime_lab import Lab

    probe = Path(build['artifacts']['probe']['path'])
    anchor = sample_clock()
    lab = Lab(w1=True, pressure=True)
    state = lab.state
    observed_processes = []
    report['lab_id'] = lab.id
    report['clock'] = anchor
    try:
        copied = lab_host_artifact(lab, build)
        with lab:
            report['build'] = versions(build, lab.tmux)
            report['exercised_artifacts'] = {'launchd_host': copied, 'probe': build['artifacts']['probe']}
            for role in bundle_roles(report['mode']):
                report['exercised_artifacts'][role] = build['artifacts'][role]
            report['agent'] = running_artifact(launchd_agent_pid(lab), copied)
            observed_processes.append(report['agent'])
            created = lab.call('create', '--cwd', state, '--', sys.executable,
                               Path(__file__).resolve(), '--fixture', state)
            runtime = created['runtime']
            pid = runtime['pane_pid']
            need(positive(pid) and uuid_text(runtime['runtime_id']), 'raw runtime did not start')
            provider = live_process(pid)
            observed_processes.append(provider)
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
                native_leg(lab, options, report, expected, anchor, provider, build)
                observed_processes.extend(data['provider'] for data in report['native']['providers'].values())
            engine = tmux_server(lab)
            need(same_process(server['process'], engine['process']) and
                 (server['started'], server['generation']) == (engine['started'], engine['generation']),
                 'dedicated tmux changed during workload')
            finished_clock = sample_clock()
            need(abs(mono(finished_clock['wall_us'], anchor) - finished_clock['mono_ns']) <= 50_000_000,
                 'wall and monotonic clocks drifted during evidence collection')
            lab_host_artifact(lab, build, copied)
            running_artifact(launchd_agent_pid(lab), copied, report['agent'])
            verify_artifacts(build, options, report['mode'], labs=(lab,))
            need(digest(Path(lab.tmux)) == report['build']['tmux_sha256'], 'tmux binary changed during workload')
    finally:
        if not lab.loaded and not lab.cleanup_confirmed and not lab.recovery:
            lab.close()
        if lab.cleanup_confirmed:
            wait(lambda: all(not same_process(item, process(item['pid'])) for item in observed_processes),
                 5, 'owned fixture or agent process survived lab cleanup')
            report['cleanup'] = 'confirmed'


def paste_workload(options, report, build, stack, observe):
    raise ProvenanceHold('prepared paste workload and live artifact observations are unavailable')


def prove_paste(options, report, build):
    artifacts = build['artifacts']
    original = None
    selected = None

    def observe(probe_pid, app_pid, hosts, previous=None):
        need(hosts, 'paste workload requires live host process observations')
        prior = previous or {}
        found = {'probe': running_artifact(probe_pid, artifacts['probe'], prior.get('probe')),
                 'app': running_artifact(app_pid, artifacts['owner']['executable'], prior.get('app'))}
        copies = {}
        for pid, (kind, lab) in hosts.items():
            if kind == 'launchd':
                artifact = copies[pid] = lab_host_artifact(lab, build, prior.get('copies', {}).get(pid))
                need(pid == launchd_agent_pid(lab), 'paste launchd copy did not serve the observed agent')
            elif kind == 'fixture' and pid == probe_pid and lab is None:
                artifact = artifacts['probe']
            else:
                need(kind == 'host' and lab is None, 'unexpected paste host role')
                artifact = artifacts['host']
            found[pid] = running_artifact(pid, artifact, prior.get(pid))
        found['copies'] = copies
        return found

    def bind(probe_pid, app_pid, hosts):
        nonlocal original, selected
        need(original is None, 'paste artifacts already bound')
        selected = (probe_pid, app_pid, dict(hosts))
        original = observe(*selected)

    try:
        with ExitStack() as stack:
            paste_workload(options, report, build, stack, bind)
            need(original is not None, 'paste workload did not bind its running artifacts')
            final = observe(*selected, previous=original)
            labs = tuple(lab for kind, lab in selected[2].values() if kind == 'launchd')
            verify_artifacts(build, options, 'paste', labs=labs)
            report['exercised_artifacts'] = {'initial': original, 'final': final}
    except ProofError as error:
        if isinstance(error, ProvenanceHold):
            raise
        raise ProvenanceHold(str(error)) from error


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--report', type=Path, help='New private JSON report destination')
    parser.add_argument('--native-evidence', type=Path, help='New same-run Root operator JSON record')
    parser.add_argument('--owner-app', type=Path, help='Signed debug owner application bundle')
    parser.add_argument('--victim-app', type=Path, help='Signed debug victim application bundle')
    parser.add_argument('--controlled-build', choices=BUILD_MODES, help='Produce a private mode-specific build record')
    parser.add_argument('--build-record', type=Path, help='Private controlled build record')
    parser.add_argument('--build-record-sha256', help='Digest retained independently from the producer completion log')
    parser.add_argument('--paste', action='store_true', help='Use the held paste acceptance route')
    parser.add_argument('--paste-native', type=Path, help='Private paste operator evidence')
    parser.add_argument('--describe-native', action='store_true', help='Print the native operator sequence and record schema')
    parser.add_argument('--fixture', type=Path, help=argparse.SUPPRESS)
    parser.add_argument('--test-record-publication', action='store_true',
                        help='Run the evidence publication regression without starting the lab')
    parser.add_argument('--test-trace-retention', action='store_true',
                        help='Run the diagnostic retention regression without starting the lab')
    options = parser.parse_args(argv)
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
    if options.controlled_build:
        if any((options.report, options.native_evidence, options.paste_native, options.paste,
                options.build_record_sha256)):
            parser.error('controlled build and acceptance arguments cannot be mixed')
        try:
            controlled_build(options)
        except (OSError, ValueError, TypeError, LookupError, ProofError, subprocess.SubprocessError) as error:
            print(str(error) if isinstance(error, ProofError) else 'Controlled build evidence incomplete.',
                  file=sys.stderr)
            return 1
        return 0
    if not options.report:
        parser.error('--report is required')
    mode = 'paste' if options.paste else 'installed' if options.native_evidence else 'bridge'
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
    report = {'format': 'w1-terminal-pressure-2', 'run_id': str(uuid.uuid4()),
              'started_ns': ns(), 'mode': mode, 'bridge': {'outcome': 'owed'},
              'native': {'outcome': 'owed'}, 'paste': {'outcome': 'owed'}}
    if options.native_evidence:
        report['native_operator_budget_seconds'] = {
            'owner_setup': NATIVE_OWNER_SETUP_SECONDS, 'inspector_setup': NATIVE_INSPECTOR_SETUP_SECONDS,
            'final_record': NATIVE_FINAL_RECORD_SECONDS, 'fixture_grace': NATIVE_FIXTURE_GRACE_SECONDS,
        }
    with output_file:
        try:
            if (bool(options.paste_native) != options.paste or options.paste and options.native_evidence
                    or any(bool(getattr(options, role + '_app')) != (role in bundle_roles(mode))
                           for role in ('owner', 'victim'))):
                raise ProvenanceHold('acceptance arguments do not match the mode')
            build = verified_build(options, mode)
            report['build_record_sha256'] = options.build_record_sha256
            need(sys.platform == 'darwin', 'pressure acceptance requires a logged-in Mac')
            if mode == 'paste':
                prove_paste(options, report, build)
            else:
                prove(options, report, build)
            verify_artifacts(build, options, mode)
            report['provenance'] = 'verified'
        except ProvenanceHold as error:
            report['failure'] = str(error)
            report['provenance'] = 'hold'
        except ProofError as error:
            report['failure'] = str(error)
            if str(error) == 'overflow not reached in actual victim WebView':
                report['native']['outcome'] = 'overflow not reached'
        except KeyboardInterrupt:
            report['failure'] = 'operator interrupted run'
        except (OSError, ValueError, TypeError, LookupError, RuntimeError, sqlite3.Error, subprocess.SubprocessError):
            report['failure'] = 'fixture, command, or evidence record was unavailable or malformed'
        report['finished_ns'] = ns()
        legs = {'bridge': ('bridge',), 'installed': ('bridge', 'native'), 'paste': ('paste',)}[mode]
        report['passed'] = ('failure' not in report and report.get('cleanup') == 'confirmed'
                            and all(report[leg]['outcome'] == 'passed' for leg in legs))
        json.dump(report, output_file, indent=2)
        output_file.write('\n')
    print('Pressure report complete.' if report['passed'] else 'Pressure evidence incomplete.', flush=True)
    return 0 if report['passed'] else 1


if __name__ == '__main__':
    sys.exit(main())
