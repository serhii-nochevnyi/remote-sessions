#!/usr/bin/env python3
"""Record W1 recovery on a logged-in Mac using the native app and disposable jobs.

Build with make build-runtime and npm run tauri -- build --debug --bundles app.
Start python3 scripts/desktop_lab.py up --w1, then run this script with --report
and an owner-approved, accessible --kingston parent under /Volumes/KINGSTON.
Each run creates and retains a disposable workspace under that parent.
Use the printed app commands in a second terminal. Quit each app when prompted.
Use normal provider-controlled login/trust and submit zero model prompts.
Finish with python3 scripts/desktop_lab.py down --w1.
"""
import argparse
from collections import Counter
from contextlib import closing, contextmanager
from datetime import datetime, timezone
from enum import Enum
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import socket
import socketserver
import sqlite3
import stat
import subprocess
import sys
import threading
from types import SimpleNamespace
from uuid import UUID, uuid4

from runtime_lifecycle import ROOT, Viewer, run, wait_for
from desktop_lab import (FIXTURE_MANIFEST, W1_MANIFEST, app_binary,
                         fixture_client_state, seed_legacy)
from runtime_lab import CATALOG_DB, Lab, W1Record, catalog_message, internal_device, private_path

REQUEST_LIMIT = 65_536
RESPONSE_LIMIT = 1_048_576
CANCEL_OPERATIONS = ('register_workspace', 'workspace_readiness', 'create_managed')
GENERATION_FORMAT = '#{pid}|#{start_time}|#{@rs_create_generation}'
PANE_FORMAT = ('#{session_name}\t#{session_id}\t#{window_id}\t#{pane_id}\t#{pane_pid}\t#{pane_dead}\t'
               '#{@rs_create_nonce}\t#{pane_start_path}\t#{pane_current_path}')
IDENTITY_FIELDS = ('session_id', 'runtime_id', 'operation_id', 'workspace_id',
                   'tmux_name', 'pane_id', 'pane_pid', 'origin')
CASE_CHECKS = {
    'R1-empty-quick-exit': ('empty', 'generation', 'one_exit', 'same_key'),
    'R1-lost-reply': ('attach_read', 'picker', 'unknown_ui', 'one_loss', 'one_create',
                      'one_process', 'same_key', 'one_tab', 'restart_identity', 'no_prompts'),
    'R1-unknown': ('picker', 'unknown_ui', 'marked', 'no_process',
                   'same_key', 'no_retry', 'unknown_after_reopen'),
    'R2-legacy': ('live', 'retained_exit', 'same_after_restart', 'native_inventory'),
    'R2-corrupt': ('blocked', 'catalog_unchanged', 'legacy_survives', 'native_error'),
    'R2-newer': ('blocked', 'catalog_unchanged', 'legacy_survives', 'native_error'),
    'R3-picker-cancel': ('cancelled', 'no_requests', 'no_effect'),
    'R3-internal-claude': ('picker', 'service_ready', 'one_launch', 'exact_cwd',
                           'provider_started', 'one_tab', 'no_prompts'),
    'R3-kingston-codex': ('picker', 'service_ready', 'one_launch', 'exact_cwd',
                          'provider_started', 'one_tab', 'no_prompts'),
    'R3-no-effect': ('picker', 'definite_ui', 'no_effect', 'no_reservation'),
    'R3-late-replacement': ('picker', 'setup_failure_ui', 'effect_barrier',
                            'retained_pane', 'no_provider'),
}


class Verdict(str, Enum):
    PASS = 'pass'
    FAIL = 'fail'
    UNRUN = 'unrun'


def case_status(case, observations):
    values = [observations.get(check) for check in CASE_CHECKS[case]]
    if any(value is False for value in values):
        return Verdict.FAIL
    return Verdict.PASS if all(value is True for value in values) else Verdict.UNRUN


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def request_frame(message):
    return json.dumps(message, separators=(',', ':')).encode() + b'\n'


def read_frame(stream, limit):
    frame = stream.readline(limit + 1)
    if len(frame) > limit or not frame.endswith(b'\n'):
        raise ValueError('Incomplete or oversized IPC frame')
    return frame


def exchange(upstream, request):
    private_path(upstream, stat.S_ISSOCK, 0o600)
    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(7)
        stream.connect(str(upstream))
        stream.sendall(request)
        with stream.makefile('rb') as reader:
            return read_frame(reader, RESPONSE_LIMIT)


def response_state(reply):
    product = reply.get('product', {})
    return product['state'] if product.get('result') == 'create' else {}


def runtime_identity(runtime):
    return {field: runtime.get(field) for field in IDENTITY_FIELDS}


def valid_id(value):
    try:
        return isinstance(value, str) and UUID(value).int != 0
    except ValueError:
        return False


def wire_object(value, required, optional=None):
    optional = optional or {}
    if (not isinstance(value, dict) or not required.keys() <= value.keys()
            or not value.keys() <= required.keys() | optional.keys()
            or any(not check(value[key]) for key, check in required.items())
            or any(key in value and not check(value[key]) for key, check in optional.items())):
        raise ValueError('Invalid IPC fields')
    return value


def object_shape(**fields):
    def matches(value):
        wire_object(value, fields)
        return True
    return matches


def variant(value, tag, variants, optional=None):
    name = value.get(tag) if isinstance(value, dict) else None
    if not isinstance(name, str) or name not in variants:
        raise ValueError('Unexpected IPC variant')
    wire_object(value, {tag: string, **variants[name]}, (optional or {}).get(name))
    return name


def choices(*allowed):
    return lambda value: isinstance(value, str) and value in allowed


def integer(bits):
    return lambda value: type(value) is int and 0 <= value < 2**bits


def nullable(check):
    return lambda value: value is None or check(value)


def array(check):
    return lambda value: isinstance(value, list) and all(check(item) for item in value)


def string(value):
    return isinstance(value, str) and not re.search(r'[\ud800-\udfff]', value)


def boolean(value):
    return type(value) is bool


def wire_uuid(value):
    if not isinstance(value, str):
        return False
    simple = r'[0-9a-fA-F]{32}'
    hyphenated = r'[0-9a-fA-F]{8}(?:-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}'
    return re.fullmatch(rf'(?:{simple}|{hyphenated}|urn:uuid:{hyphenated}|\{{{hyphenated}\}})', value) is not None


def inventory_uuid(value):
    return isinstance(value, str) and value.isascii() and wire_uuid(value.lower())


def runtime_shape(value, uuid_check=wire_uuid):
    wire_object(value, {'session_id': uuid_check, 'runtime_id': uuid_check, 'tmux_name': string,
                        'pane_id': string, 'command': string, 'exited': boolean,
                        'width': U16, 'height': U16},
                {'pane_pid': nullable(U32), 'origin': choices('managed', 'legacy'),
                 'workspace_id': nullable(uuid_check), 'operation_id': nullable(uuid_check)})
    session_id = UUID(value['session_id'].lower())
    runtime_id = UUID(value['runtime_id'].lower())
    if (not session_id.int or not runtime_id.int
            or not value['tmux_name'].startswith('rs_')
            or value['tmux_name'][3:].lower() != f'{session_id.hex}_{runtime_id.hex}'
            or not re.fullmatch(r'%(?:0|[1-9][0-9]{0,9})', value['pane_id'])
            or not U32(int(value['pane_id'][1:]))):
        raise ValueError('Invalid IPC runtime identity')
    return True


def desktop_pid(value):
    return U32(value) and 1 <= value <= 2**31


def inventory_shape(rows):
    if not isinstance(rows, list) or len(rows) > 16:
        return False
    runtime_ids = set()
    for row in rows:
        runtime_shape(row, inventory_uuid)
        runtime_id = str(UUID(row['runtime_id'].lower()))
        if runtime_id in runtime_ids:
            return False
        runtime_ids.add(runtime_id)

        workspace_id, operation_id = row.get('workspace_id'), row.get('operation_id')
        if workspace_id is not None or operation_id is not None:
            if (row.get('origin', 'legacy') != 'managed'
                    or not all(value is not None and valid_id(value.lower())
                               for value in (workspace_id, operation_id))):
                return False
        if (not nullable(desktop_pid)(row.get('pane_pid'))
                or not 1 <= row['width'] <= 10_000 or not 1 <= row['height'] <= 10_000
                or len(row['command'].encode('utf-16-le')) > 512):
            return False
    return True


def resize_shape(value):
    result = variant(value, 'result', {
        'busy': {'terminal': TERMINAL, 'size': SIZE},
        'committed': {'admission': INTENT, 'status': TERMINAL_STATUS},
        'rejected': {'terminal': TERMINAL, 'intent': INTENT},
        'uncertain': {'terminal': TERMINAL, 'intent': INTENT},
    })
    if result == 'busy' and not (2 <= value['size']['cols'] <= 500 and 2 <= value['size']['rows'] <= 200):
        raise ValueError('Unsupported IPC resize size')
    return True


def terminal_identity(terminal):
    target = terminal['target']
    return {
        **terminal,
        'view_id': UUID(terminal['view_id']),
        'agent_epoch': UUID(terminal['agent_epoch']),
        'target': {**target, 'runtime_id': UUID(target['runtime_id']), 'generation': UUID(target['generation'])},
    }


def launch_outcome_shape(value):
    wire_object(value, {'runtime': runtime_shape}, {'setup_issue': nullable(ISSUE)})
    return True


def create_state_shape(value):
    nullable_issue = nullable(ISSUE)
    variant(value, 'state', {
        'absent': {},
        'pending': {'reservation': wire_uuid},
        'failed': {'issue': ISSUE},
        'unknown': {},
        'confirmed': {'outcome': launch_outcome_shape, 'current': choices('live', 'exited', 'unavailable')},
    }, {'unknown': {'issue': nullable_issue}, 'confirmed': {'setup_issue': nullable_issue}})
    return True


U16, U32, U64 = (integer(bits) for bits in (16, 32, 64))
DATA = array(integer(8))
SIZE = object_shape(cols=U16, rows=U16)
TARGET = object_shape(runtime_id=wire_uuid, generation=wire_uuid, session=string, window=string,
                      pane=string, pid=U32, process_start=U64)
TERMINAL = object_shape(target=TARGET, view_id=wire_uuid, agent_epoch=wire_uuid)
EPOCH = object_shape(agent=wire_uuid, owner=wire_uuid)
INTENT = object_shape(epoch=EPOCH, id=wire_uuid)
GEOMETRY = object_shape(size=SIZE, revision=U64, intent=nullable(INTENT))
PROVIDER = object_shape(size=SIZE, sampled_at_us=U64)
TERMINAL_STATUS = object_shape(terminal=TERMINAL, epoch=nullable(EPOCH), committed=GEOMETRY,
                               provider=nullable(PROVIDER), observation=choices('pending', 'observed', 'unavailable'))
BINDING = object_shape(terminal=TERMINAL, token=wire_uuid, epoch=EPOCH, incarnation=wire_uuid,
                       operation=U64, sequence=U64)
PROFILE = choices('claude_code', 'codex')
ISSUE = choices(*('invalid_request invalid_folder unsupported_path folder_unavailable folder_access_denied '
                  'workspace_unknown workspace_changed workspace_check_timed_out provider_missing '
                  'provider_not_executable interpreter_unavailable catalog_unconfigured catalog_missing '
                  'catalog_unsafe catalog_busy catalog_unsupported catalog_unavailable create_conflict '
                  'runtime_limit tmux_unavailable launch_setup_failed pending_unavailable').split())
RUNTIME_ID = {'runtime_id': wire_uuid}
LEASE = {**RUNTIME_ID, 'token': wire_uuid}
REQUESTS = {
    'health': {}, 'list': {},
    'create': {**RUNTIME_ID, 'session_id': wire_uuid, 'cwd': string, 'argv': array(string)},
    'snapshot': RUNTIME_ID, 'attach': RUNTIME_ID, 'acquire': RUNTIME_ID,
    'open_terminal': {**RUNTIME_ID, 'view_id': wire_uuid},
    'acquire_terminal': {'terminal': TERMINAL}, 'terminal_status': {'terminal': TERMINAL},
    'resize_terminal': {'terminal': TERMINAL, 'token': wire_uuid, 'sequence': U64, 'size': SIZE},
    'renew': LEASE, 'release': LEASE,
    'input': {**LEASE, 'sequence': U64, 'data': DATA},
    'paste_stage': {'binding': BINDING, 'offset': U64, 'data': DATA},
    'paste_commit': {'binding': BINDING, 'bytes': U64},
    'paste_ack': {'binding': BINDING, 'receipt': wire_uuid},
    'stop': {**RUNTIME_ID, 'expected_pane_id': string},
}
PRODUCT_REQUESTS = {
    'register_workspace': {'locator': string},
    'workspace_readiness': {'workspace_id': wire_uuid, 'profile': PROFILE},
    'create_managed': {'key': wire_uuid, 'workspace_id': wire_uuid, 'profile': PROFILE},
    'create_status': {'key': wire_uuid},
}
REPLIES = {
    'health': {'agent_pid': U32, 'remote_execution_enabled': boolean},
    'runtimes': {'runtimes': inventory_shape},
    'created': {'runtime': runtime_shape},
    'snapshot': {'text': string},
    'attachment': {'tmux_name': string},
    'control': {'token': wire_uuid},
    'terminal_opened': {'status': TERMINAL_STATUS},
    'terminal_control': {'token': wire_uuid, 'epoch': EPOCH},
    'terminal_status': {'status': TERMINAL_STATUS},
    'terminal_changed': {},
    'control_recovering': {'paste_admission': choices('prior_send_unknown', 'unavailable')},
    'paste_unavailable': {},
    'paste': {'paste': lambda value: isinstance(value, dict)},
    'resize': {'resize': resize_shape},
    'accepted': {},
    'stopped': RUNTIME_ID,
    'error': {'message': string},
}
FAMILIES = {
    'health': ('health',), 'list': ('runtimes',), 'create': ('created',),
    'snapshot': ('snapshot',), 'attach': ('attachment',), 'acquire': ('control', 'control_recovering'),
    'open_terminal': ('terminal_opened',), 'acquire_terminal': ('terminal_control', 'control_recovering'),
    'terminal_status': ('terminal_status', 'terminal_changed'), 'resize_terminal': ('resize',),
    'renew': ('accepted', 'control_recovering'), 'release': ('accepted',),
    'input': ('accepted', 'control_recovering'), 'stop': ('stopped',),
    'paste_stage': ('paste',), 'paste_commit': ('paste',), 'paste_ack': ('paste',),
}
PASTE_REPLIES = {
    'paste_stage': {'staged': {}, 'stale': {}, 'invalid': {}, 'unavailable': {}, 'unknown': {}},
    'paste_commit': {'committed': {'receipt': wire_uuid}, 'stale': {}, 'invalid': {},
                     'unavailable': {}, 'unknown': {}},
    'paste_ack': {'acknowledged': {'writable': boolean}, 'stale': {}, 'unknown': {}},
}
PRODUCT_REPLIES = {
    'workspace_registered': {'workspace': object_shape(workspace_id=wire_uuid, label=string)},
    'ready': {}, 'blocked': {'issue': ISSUE}, 'create': {'state': create_state_shape},
}
PRODUCT_FAMILIES = {
    'register_workspace': 'workspace_registered', 'workspace_readiness': 'ready',
    'create_managed': 'create', 'create_status': 'create',
}


def decode_ipc(frame):
    def pairs(items):
        value = dict(items)
        if len(value) != len(items):
            raise ValueError('Duplicate IPC field')
        return value

    def invalid_constant(_):
        raise ValueError('Invalid IPC number')

    return json.loads(frame.decode('utf-8'), object_pairs_hook=pairs, parse_constant=invalid_constant)


def validate_request(message):
    if isinstance(message, dict) and 'product' in message:
        wire_object(message, {'product': lambda value: isinstance(value, dict)})
        variant(message['product'], 'op', PRODUCT_REQUESTS,
                {'create_managed': {'reservation': nullable(wire_uuid)}})
    else:
        variant(message, 'op', REQUESTS, {'input': {'paste': boolean}})


def validate_reply(message, reply):
    if 'product' in message and isinstance(reply, dict) and 'product' in reply:
        wire_object(reply, {'product': lambda value: isinstance(value, dict)})
        op = message['product']['op']
        result = variant(reply['product'], 'result',
                         {name: PRODUCT_REPLIES[name] for name in (PRODUCT_FAMILIES[op], 'blocked')})
        if result == 'blocked' and op in ('create_managed', 'create_status'):
            if reply['product']['issue'] != 'catalog_unconfigured':
                raise ValueError('Unexpected IPC create error')
        return result
    if 'product' in message:
        return variant(reply, 'result', {'error': REPLIES['error']})
    op = message['op']
    names = FAMILIES[op]
    if op == 'input' and message.get('paste') is True:
        names = ('paste_unavailable',)
    result = variant(reply, 'result', {name: REPLIES[name] for name in (*names, 'error')})
    if result == 'health' and not desktop_pid(reply['agent_pid']):
        raise ValueError('Invalid desktop inventory agent PID')
    if result == 'resize' and reply['resize']['result'] == 'busy':
        resize = reply['resize']
        if (resize['size'] != message['size']
                or terminal_identity(resize['terminal']) != terminal_identity(message['terminal'])):
            raise ValueError('Mismatched IPC resize Busy reply')
    if result == 'paste':
        variant(reply['paste'], 'result', PASTE_REPLIES[op])
    return result


class ResponseLoss:
    """Drop one authoritative live create result after it has arrived from the lab."""

    def __init__(self):
        self.lock = threading.Lock()
        self.profile = None
        self.drops = []
        self.requests = Counter()
        self.replies = Counter()
        self.failures = 0
        self.active = 0

    def arm(self, profile):
        with self.lock:
            require(self.profile is None and not self.drops, 'Response loss is one-shot')
            self.profile = profile

    def disarm(self):
        with self.lock:
            self.profile = None

    def request(self, message):
        request = message.get('product', message)
        op = request.get('op')
        if op in ('create_managed', 'create_status', 'register_workspace', 'workspace_readiness', 'list'):
            with self.lock:
                self.requests[op, request.get('key')] += 1

    def count(self, op, key=None):
        with self.lock:
            if key is None:
                return sum(n for (name, _), n in self.requests.items() if name == op)
            return self.requests[op, key]

    def response(self, message, frame):
        if len(frame) > RESPONSE_LIMIT or not frame.endswith(b'\n'):
            raise ValueError('Incomplete or oversized upstream response')
        request = message.get('product', message)
        op, key = request.get('op'), request.get('key')
        reply = decode_ipc(frame)
        result = validate_reply(message, reply)
        state = response_state(reply)
        runtime = state.get('outcome', {}).get('runtime', {})
        with self.lock:
            if (op == 'create_managed' and self.profile is not None and request.get('profile') == self.profile
                    and state.get('state') == 'confirmed' and state.get('current') == 'live'
                    and runtime.get('workspace_id') == request.get('workspace_id')
                    and all(valid_id(runtime.get(field)) for field in
                            ('workspace_id', 'session_id', 'runtime_id', 'operation_id'))
                    and valid_id(key) and runtime.get('origin') == 'managed'
                    and runtime.get('tmux_name') == ('rs_' + UUID(runtime['session_id']).hex
                                                     + '_' + UUID(runtime['runtime_id']).hex)
                    and re.fullmatch(r'%[0-9]+', runtime.get('pane_id', ''))
                    and type(runtime.get('pane_pid')) is int and runtime['pane_pid'] > 0
                    and runtime.get('exited') is False):
                self.drops.append({'key': key, 'runtime': runtime})
                self.profile = None
                return None, None
        credit = (op, key) if (op, result) in (('list', 'runtimes'), ('create_status', 'create')) else None
        return frame, credit

    def relay(self, reader, upstream):
        request = read_frame(reader, REQUEST_LIMIT)
        message = decode_ipc(request)
        validate_request(message)
        self.request(message)
        frame = exchange(upstream, request)
        return self.response(message, frame)

    def forwarded(self, op, key=None):
        with self.lock:
            return self.replies[op, key]

    def cancel_snapshot(self):
        with self.lock:
            return {
                'requests': {op: sum(n for (name, _), n in self.requests.items() if name == op)
                             for op in CANCEL_OPERATIONS},
                'forwarded_lists': self.replies['list', None],
                'failures': self.failures,
                'active': self.active,
                'pass_through': self.profile is None and not self.drops,
            }


@contextmanager
def agent_proxy(lab, directory, loss):
    upstream = lab.state / 'agent.sock'

    class Handler(socketserver.StreamRequestHandler):
        def handle(self):
            with loss.lock:
                loss.active += 1
            credit = None
            failed = True
            try:
                self.request.settimeout(7)
                response, eligible = loss.relay(self.rfile, upstream)
                if response is not None:
                    self.request.sendall(response)
                    credit = eligible
                failed = False
            except (OSError, TypeError, ValueError, RecursionError):
                pass
            finally:
                with loss.lock:
                    loss.active -= 1
                    loss.failures += failed
                    if credit is not None:
                        loss.replies[credit] += 1

    path = directory / 'agent.sock'
    server = socketserver.ThreadingUnixStreamServer(str(path), Handler)
    try:
        with server:
            path.chmod(0o600)
            worker = threading.Thread(target=server.serve_forever, kwargs={'poll_interval': 0.1})
            worker.start()
            try:
                yield
            finally:
                server.shutdown()
                worker.join()
    finally:
        path.unlink(missing_ok=True)


PROVIDER = r'''
import json
import os
from pathlib import Path
import time

state = Path(__file__).parent
cwd = Path.cwd()
identity = cwd.stat()
with (state / 'provider-starts.jsonl').open('a') as output:
    output.write(json.dumps(dict(cwd=str(cwd), device=identity.st_dev,
                                inode=identity.st_ino, pid=os.getpid())) + '\n')
print('W1 deterministic provider witness', flush=True)
if cwd.name == 'quick-exit':
    raise SystemExit(0)
while True:
    time.sleep(1)
'''

TMUX_BARRIER = r'''
import json
import os
from pathlib import Path
import re
import sys

state = Path(__file__).parent
tmux = (state / 'tmux-executable').read_text()
args = sys.argv[1:]
if args[:3] != ['-N', '-S', str(state / 'tmux.sock')]:
    raise SystemExit('unexpected fixture endpoint')
arm = state / 'effect-barrier.json'
if len(args) == 7 and args[3:5] == ['if-shell', '-F'] and arm.exists():
    target = re.match(r'new-session -d -E -s (rs_[a-f0-9]{32}_[a-f0-9]{32}) ', args[6])
    if target and ' __managed-launch ' in args[6]:
        action = json.loads(arm.read_text())
        arm.unlink()
        event = dict(action=action, target=target[1])
        if action == 'replacement':
            folder = state / 'late-replacement'
            original = folder.stat()
            folder.rename(state / 'late-replacement-original')
            folder.mkdir(mode=0o700)
            event.update(original_inode=original.st_ino, replacement_inode=folder.stat().st_ino)
        elif action != 'unknown':
            raise SystemExit('unexpected fixture barrier')
        with (state / 'effects.jsonl').open('a') as output:
            output.write(json.dumps(event) + '\n')
        if action == 'unknown':
            raise SystemExit(1)
os.execv(tmux, [tmux, *args])
'''


def install_fixture(lab):
    bin_dir = lab.state / 'bin'
    bin_dir.mkdir(mode=0o700)
    python = shlex.quote(str(Path(sys.executable).resolve(strict=True)))
    for executable, source in [(bin_dir / 'codex', PROVIDER),
                               (lab.state / 'tmux-fixture', TMUX_BARRIER)]:
        script = lab.state / (executable.name + '.py')
        script.write_text(source)
        executable.write_text(f'#!/bin/sh\nexec {python} {shlex.quote(str(script))} "$@"\n')
        executable.chmod(0o700)
    (lab.state / 'tmux-executable').write_text(lab.tmux)
    lab.agent_path = os.pathsep.join(map(str, (bin_dir, '/usr/bin', '/bin')))


@contextmanager
def fixture_jobs():
    require(not os.path.lexists(FIXTURE_MANIFEST),
            'Earlier fixture recorded. Close its app and run desktop_lab.py down --w1 --fixture')
    lab = Lab(w1=True, fixture=True, manifest=FIXTURE_MANIFEST)
    try:
        install_fixture(lab)
    except (Exception, KeyboardInterrupt):
        lab.close()
        raise
    lab.start()
    try:
        lab.publish_w1_manifest()
        yield W1Record(FIXTURE_MANIFEST, fixture=True)
    finally:
        lab.close()


def product(lab, op, **fields):
    return json.loads(exchange(lab.state / 'agent.sock',
                               request_frame({'product': {'op': op, **fields}})))


def inventory(lab):
    return {r['runtime_id']: r for r in lab.call('list')['runtimes']}


def tmux_text(lab, *args):
    result = lab.tmux_call(*args)
    if result.returncode:
        raise RuntimeError(f'tmux {args[0]} failed ({result.returncode})')
    return result.stdout.rstrip('\n')


def generation(lab):
    pid, started, token = tmux_text(lab, 'display-message', '-p', GENERATION_FORMAT).split('|')
    return {'pid': int(pid), 'start': int(started), 'token': token}


def process_start(pid):
    started = run('ps', '-p', pid, '-o', 'lstart=').stdout.strip()
    require(bool(started), f'No process start time for PID {pid}')
    return started


def pane(lab, runtime):
    fields = tmux_text(lab, 'display-message', '-p', '-t', runtime['pane_id'], PANE_FORMAT)
    name, session, window, pane_id, pid, dead, stamp, start_cwd, cwd = fields.split('\t')
    pid = int(pid) if pid else None
    require(name == runtime['tmux_name'] and pane_id == runtime['pane_id'] and dead in ('0', '1'),
            'Pane metadata did not match the runtime target')
    require((dead == '1' and pid is None) or (pid is not None and pid == runtime['pane_pid']),
            'Pane PID did not match the runtime')
    nonce, launch_pid = None, None
    if runtime['origin'] == 'managed':
        nonce, stamped_session, stamped_window, stamped_pane, stamped_pid = stamp.split(':')
        launch_pid = int(stamped_pid)
        require(valid_id(nonce) and launch_pid > 0 and (pid is None or pid == launch_pid)
                and (stamped_session, stamped_window, stamped_pane) == (session, window, pane_id),
                'Managed pane launch stamp did not match the runtime')
    return {'session': session, 'window': window, 'pane': pane_id, 'pid': pid,
            'dead': dead == '1', 'nonce': nonce, 'launch_pid': launch_pid,
            'start_cwd': start_cwd, 'cwd': cwd, 'started': process_start(pid) if dead == '0' else None}


def checked_runtime(lab, runtime):
    return dict(runtime_identity(runtime), pane=pane(lab, runtime))


def path_identity(path):
    metadata = path.stat()
    return {'path': str(path.resolve()), 'device': metadata.st_dev, 'inode': metadata.st_ino}


def digest(path):
    result = hashlib.sha256()
    with path.open('rb') as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b''):
            result.update(chunk)
    return result.hexdigest()


def catalog_snapshot(lab):
    db = lab.catalog / CATALOG_DB
    provision = lab.catalog / 'catalog.provision'
    for path in (db, provision):
        private_path(path, stat.S_ISREG, 0o600)
    with closing(sqlite3.connect(db.as_uri() + '?mode=ro', uri=True)) as connection:
        persisted = {name: connection.execute('PRAGMA ' + name).fetchone()[0]
                     for name in ('application_id', 'user_version', 'journal_mode')}
        operations = connection.execute(
            'SELECT o.key, o.id, o.stage, o.profile, s.workspace_id, s.id, a.id, a.target, '
            'a.nonce, a.generation, a.policy FROM create_operations o JOIN runtime_attempts a '
            'ON o.runtime_id=a.id JOIN sessions s ON s.id=a.session_id ORDER BY o.key'
        ).fetchall()
        workspaces = [row[0] for row in connection.execute('SELECT id FROM workspaces ORDER BY id')]
    saved = {}
    for key, operation, stage, profile, workspace, session, runtime, target, nonce, server, policy_json in operations:
        policy = json.loads(policy_json)
        saved[key] = {
            'operation_id': operation, 'stage': stage, 'profile': json.loads(profile), 'workspace_id': workspace,
            'session_id': session, 'runtime_id': runtime, 'target': target, 'nonce': nonce,
            'generation': json.loads(server), 'workspace': policy['workspace'],
            'executable': policy['executable'], 'executable_identity': policy['executable_identity'],
        }
    return {
        'identity': path_identity(db), 'provision': path_identity(provision),
        'settings': persisted, 'service_settings': catalog_message(lab.state),
        'workspace_ids': workspaces, 'operations': saved,
    }


def pending_records(directory):
    records = {}
    for path in (directory / 'pending-create').glob('operation-*'):
        private_path(path, stat.S_ISREG, 0o600)
        version, key, workspace, profile, previous, presented, submission, *extra = path.read_text().splitlines()
        reference = path.name.removeprefix('operation-')
        require(version == 'remote-sessions-create-1' and all(map(valid_id, (reference, key, workspace)))
                and reference != key and profile in ('claude_code', 'codex') and key not in records
                and (previous == '-' or valid_id(previous) and previous != reference)
                and presented in ('shown', 'unshown') and submission in ('fresh', 'sent', 'rejected')
                and len(extra) == int(submission == 'rejected'),
                'Unexpected native pending record')
        records[key] = {'reference': reference, 'workspace_id': workspace, 'profile': profile,
                        'previous': previous, 'presented': presented, 'submission': submission,
                        'issue': json.loads(extra[0]) if submission == 'rejected' else None}
    return records


def records(path):
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines()]


def app_command(*args):
    return [sys.executable, str(ROOT / 'scripts/desktop_lab.py'), *args]


def app_prompt(case, *args):
    command = app_command('run', '--w1', *args)
    case['commands'].append({'argv': command, 'operator': True})
    print('\nRun this command in a second Terminal window.\n  ' + shlex.join(command), flush=True)


def observe(case, name, prompt):
    while True:
        answer = input(prompt + ' [yes/no/unrun] ').strip().lower()
        if answer in ('yes', 'no', 'unrun'):
            case['checks'][name] = {'yes': True, 'no': False, 'unrun': None}[answer]
            return


def capture_restart(case, lab):
    command = ['launchctl', 'kickstart', '-k', lab.targets['agent']]
    entry = {'argv': command}
    case['commands'].append(entry)
    before = process_start(lab.loaded_pid('agent'))
    offset = (lab.state / 'agent.stderr.log').stat().st_size
    pids = lab.restart_agent()
    entry['pids'] = pids
    entry['process_started'] = [before, process_start(pids[1])]
    entry['catalog_startup'] = wait_for(lambda: catalog_message(lab.state, offset))


def mark(case, **observations):
    case['checks'].update({key: bool(value) for key, value in observations.items()})


@contextmanager
def scenario(report, name):
    case = report['cases'][name]
    print('\n' + name, flush=True)
    try:
        yield case
    except (KeyboardInterrupt, EOFError):
        case['reason'] = 'Operator stopped the procedure'
        raise
    except (OSError, LookupError, TypeError, ValueError, RuntimeError,
            sqlite3.Error, subprocess.SubprocessError) as error:
        case['status'] = Verdict.FAIL
        case['reason'] = str(error)
        print('Case failed: ' + str(error), file=sys.stderr)
    else:
        case['status'] = case_status(name, case['checks'])


def wait_for_pending(directory, previous):
    def new_key():
        additions = pending_records(directory).keys() - previous.keys()
        require(len(additions) <= 1, 'More than one new native request key')
        return next(iter(additions), None)
    return wait_for(new_key)


def stop_app():
    input('Quit Remote Sessions (Cmd+Q) in the other window, then press Enter here. ')


def empty_quick_exit(report, lab):
    with scenario(report, 'R1-empty-quick-exit') as case:
        folder = lab.state / 'quick-exit'
        folder.mkdir(mode=0o700)
        case['before'] = generation(lab)
        mark(case, empty=not tmux_text(lab, 'list-sessions', '-F', '#{session_name}')
             and not inventory(lab) and not case['before']['token'])
        workspace = product(lab, 'register_workspace', locator=str(folder))['product']['workspace']['workspace_id']
        message = {'key': str(uuid4()), 'workspace_id': workspace, 'profile': 'codex'}
        initial = response_state(product(lab, 'create_managed', **message))
        key = message['key']

        def exited():
            current = response_state(product(lab, 'create_status', key=key))
            return current if current.get('state') == 'confirmed' and current.get('current') == 'exited' else None

        status = wait_for(exited)
        repeat = response_state(product(lab, 'create_managed', **message))
        runtime = status['outcome']['runtime']
        observed = checked_runtime(lab, runtime)
        case.update(key=key, first=initial, status_reply=status, replay=repeat, runtime=observed,
                    after=generation(lab), starts=records(lab.state / 'provider-starts.jsonl'),
                    catalog=catalog_snapshot(lab))
        same_server = ('pid', 'start')
        mark(case, generation=all(case['before'][field] == case['after'][field] for field in same_server)
             and valid_id(case['after']['token']),
             one_exit=len(case['starts']) == 1 and case['starts'][0]['pid'] == observed['pane']['launch_pid']
             and case['starts'][0]['cwd'] == observed['pane']['start_cwd'] == str(folder)
             and observed['pane']['dead'] and set(inventory(lab)) == {runtime['runtime_id']}
             and status['setup_issue'] is None,
             same_key=repeat == status and list(case['catalog']['operations']) == [key])


@contextmanager
def native(case, *args):
    app_prompt(case, *args)
    try:
        yield
    except (KeyboardInterrupt, EOFError):
        raise
    except Exception:
        stop_app()
        raise
    else:
        stop_app()


def select_workspace(case, folder, profile):
    case['selected_folder'] = path_identity(folder)
    observe(case, 'picker', f'In Create Session, choose {folder} with the native picker and select '
                           f'{profile}. Did you see that folder and a ready profile?')


def submit_native(case, directory, previous):
    input('Click Create Session once, then press Enter. ')
    key = wait_for_pending(directory, previous)
    case['key'] = key
    case['pending'] = pending_records(directory)[key]
    return key


def add_legacy(lab):
    live = seed_legacy(lab)
    exited = seed_legacy(lab, suffix='-exit')
    before_exit = checked_runtime(lab, exited)
    tmux_text(lab, 'send-keys', '-t', exited['pane_id'], 'C-c')
    waited = wait_for(lambda: inventory(lab)[exited['runtime_id']]['exited'])
    require(waited, 'Legacy diagnostic did not exit')
    return [live['runtime_id'], exited['runtime_id']], before_exit


def legacy_snapshot(lab, ids):
    current = inventory(lab)
    return [checked_runtime(lab, current[runtime_id]) for runtime_id in ids]


def legacy_case(report, lab, ids, before_exit, app_args):
    with scenario(report, 'R2-legacy') as case:
        before = legacy_snapshot(lab, ids)
        case['before'] = before
        case['before_diagnostic_exit'] = before_exit
        legacy = all(r['origin'] == 'legacy' and r['workspace_id'] is None
                     and r['operation_id'] is None for r in before)
        same_pane = all(before_exit['pane'][field] == before[1]['pane'][field]
                        for field in ('session', 'window', 'pane'))
        mark(case, live=legacy and not before[0]['pane']['dead'],
             retained_exit=legacy and same_pane and not before_exit['pane']['dead']
             and before[1]['pane']['dead'] and before[1]['runtime_id'] == before_exit['runtime_id'])
        capture_restart(case, lab)
        case['after'] = legacy_snapshot(lab, ids)
        mark(case, same_after_restart=case['after'] == before)
        with native(case, *app_args):
            observe(case, 'native_inventory',
                    'Does the native app list the live diagnostic and the distinct exited diagnostic? '
                    'Can you open the live one and read its P0 runtime alive witness?')


def attach_preflight(lab, directory, runtime_id):
    state = SimpleNamespace(state=directory, host=lab.host, tmux=lab.tmux)
    require((directory / 'tmux.sock').samefile(lab.state / 'tmux.sock'),
            'The client tmux socket does not resolve to the fixture job')
    viewer = Viewer(state, runtime_id)
    try:
        wait_for(viewer.read, timeout=5)
    finally:
        viewer.close()
    return {'runtime_id': runtime_id, 'socket': str(directory / 'tmux.sock'), 'witness_read': True}


def lost_reply_case(report, lab, client, loss, preflight, app_args):
    with scenario(report, 'R1-lost-reply') as case:
        case['preflight'] = preflight
        mark(case, attach_read=preflight['witness_read'])
        folder = lab.state / 'lost reply Проба'
        folder.mkdir(mode=0o700)
        starts_before = records(lab.state / 'provider-starts.jsonl')
        operations_before = pending_records(client)
        inventory_before = inventory(lab)
        creates_before = loss.count('create_managed')
        lists_before = loss.forwarded('list')
        with native(case, *app_args):
            select_workspace(case, folder, 'Codex')
            loss.arm('codex')
            key = submit_native(case, client, operations_before)
            observe(case, 'unknown_ui', 'Did the dialog report an unknown launch result?')
            require(len(loss.drops) == 1, 'Expected one authoritative create result to be dropped')
            case['drop'] = loss.drops[0]
            case['original'] = checked_runtime(lab, case['drop']['runtime'])

        case['catalog_before_restart'] = catalog_snapshot(lab)
        case['tmux_before_restart'] = generation(lab)
        case['status_before_restart'] = response_state(product(lab, 'create_status', key=key))
        status_before = loss.forwarded('create_status', key)
        capture_restart(case, lab)
        with native(case, *app_args):
            input('Open the Create Session dialog and refresh the original attempt if needed. '
                  'Open its terminal once if it did not reopen automatically. Press Enter here. ')
            observe(case, 'one_tab', 'Does the original attempt resolve to exactly one tab with the W1 witness?')
            observe(case, 'no_prompts', 'Did this run submit zero model prompts?')
        case['status_after_restart'] = response_state(product(lab, 'create_status', key=key))
        case['catalog_after_restart'] = catalog_snapshot(lab)
        case['tmux_after_restart'] = generation(lab)
        case['pending_after_restart'] = pending_records(client)[key]
        now = inventory(lab)
        case['reopened'] = checked_runtime(lab, now[case['original']['runtime_id']])
        starts = records(lab.state / 'provider-starts.jsonl')
        case['new_starts'] = starts[len(starts_before):]
        case['counts'] = {'dropped': len(loss.drops),
                          'native_creates': loss.count('create_managed') - creates_before,
                          'same_key_creates': loss.count('create_managed', key),
                          'same_key_status_after_restart': loss.forwarded('create_status', key) - status_before,
                          'inventory_replies': loss.forwarded('list') - lists_before,
                          'new_runtimes': len(now.keys() - inventory_before.keys())}
        operation = case['catalog_after_restart']['operations'][key]
        mark(case, one_loss=case['counts']['dropped'] == 1 and case['drop']['key'] == key,
             one_create=case['counts']['native_creates'] == case['counts']['same_key_creates'] == 1,
             one_process=len(case['new_starts']) == case['counts']['new_runtimes'] == 1
             and case['new_starts'][0]['pid'] == case['original']['pane_pid']
             and case['new_starts'][0]['cwd'] == str(folder)
             and case['new_starts'][0]['inode'] == case['selected_folder']['inode'],
             same_key=case['counts']['same_key_status_after_restart'] >= 1
             and case['counts']['inventory_replies'] >= 1
             and case['pending_after_restart']['reference'] == case['pending']['reference']
             and case['pending_after_restart']['workspace_id'] == operation['workspace_id']
             and operation['operation_id'] == case['original']['operation_id']
             and operation['runtime_id'] == case['original']['runtime_id']
             and operation['stage'] == 'finished',
             restart_identity=case['tmux_before_restart'] == case['tmux_after_restart']
             and case['catalog_before_restart'] == case['catalog_after_restart']
             and case['status_after_restart'] == case['status_before_restart']
             and case['status_after_restart'].get('current') == 'live'
             and case['reopened'] == case['original'])


def fault_launch(report, lab, client, loss, app_args, name, action):
    with scenario(report, name) as case:
        folder = lab.state / {'definite': 'preflight-replacement', 'replacement': 'late-replacement',
                              'unknown': 'unknown-effect'}[action]
        folder.mkdir(mode=0o700)
        starts = records(lab.state / 'provider-starts.jsonl')
        effects = records(lab.state / 'effects.jsonl')
        previous = pending_records(client)
        before = inventory(lab)
        creates_before = loss.count('create_managed')
        status_before = None
        with native(case, *app_args):
            select_workspace(case, folder, 'Codex')
            if action == 'definite':
                original = folder.with_name(folder.name + '-original')
                folder.rename(original)
                folder.mkdir(mode=0o700)
                case['replacement'] = path_identity(folder)
            else:
                (lab.state / 'effect-barrier.json').write_text(json.dumps(action))
            key = submit_native(case, client, previous)
            if action == 'definite':
                observe(case, 'definite_ui', 'Does the app report that the chosen folder changed and must be reselected?')
            elif action == 'replacement':
                observe(case, 'setup_failure_ui', 'Does the app report provider setup failure and no ready terminal?')
            else:
                observe(case, 'unknown_ui', 'Does the app report the launch result as unknown?')
                status_before = loss.forwarded('create_status', key)
                input('Refresh that attempt once without creating a separate session. Press Enter here. ')

        first = response_state(product(lab, 'create_status', key=key))
        case['status_reply'] = first
        case['pending'] = pending_records(client)[key]
        case['catalog'] = catalog_snapshot(lab)
        operation = case['catalog']['operations'].get(key)
        case['effects'] = records(lab.state / 'effects.jsonl')[len(effects):]
        case['starts'] = records(lab.state / 'provider-starts.jsonl')[len(starts):]
        now = inventory(lab)
        case['new_runtimes'] = sorted(now.keys() - before.keys())
        case['native_creates'] = loss.count('create_managed') - creates_before
        single_create = case['native_creates'] == loss.count('create_managed', key) == 1
        if action == 'definite':
            mark(case, no_effect=single_create and not case['starts'] and not case['effects']
                 and now.keys() == before.keys()
                 and case['selected_folder']['inode'] != case['replacement']['inode']
                 and case['pending']['submission'] == 'rejected'
                 and case['pending']['issue'] == 'workspace_changed',
                 no_reservation=operation is None and first.get('state') == 'absent')
        elif action == 'replacement':
            runtime = first.get('outcome', {}).get('runtime', {})
            require(operation and runtime, 'No retained launch identity for the replacement attempt')
            observed = checked_runtime(lab, runtime)
            case['runtime'] = observed
            event = case['effects'][0] if len(case['effects']) == 1 else {}
            mark(case, effect_barrier=single_create and event.get('action') == action
                 and event.get('target') == operation['target']
                 and event.get('original_inode') == operation['workspace']['inode'] == case['selected_folder']['inode']
                 and event.get('replacement_inode') == folder.stat().st_ino != event.get('original_inode'),
                 retained_pane=case['new_runtimes'] == [operation['runtime_id']]
                 and first['current'] == 'exited' and first['setup_issue'] == 'workspace_changed'
                 and observed['pane']['dead'] and observed['pane']['start_cwd'] == str(folder)
                 and observed['pane']['nonce'] == operation['nonce']
                 and runtime['operation_id'] == operation['operation_id'],
                 no_provider=not case['starts'])
        else:
            event = case['effects'][0] if len(case['effects']) == 1 else {}
            mark(case, marked=operation and operation['stage'] == 'marked'
                 and first.get('state') == 'unknown'
                 and event.get('action') == action and event.get('target') == operation['target'])
            status_before_restart = loss.forwarded('create_status', key)
            case['same_key_status_before_restart'] = status_before_restart - status_before
            capture_restart(case, lab)
            with native(case, *app_args):
                observe(case, 'unknown_after_reopen', 'After reopening and refreshing the same attempt, '
                        'is it still unknown with no new tab?')
            case['status_after_restart'] = response_state(product(lab, 'create_status', key=key))
            case['pending_after_restart'] = pending_records(client)[key]
            case['catalog_after_restart'] = catalog_snapshot(lab)
            case['same_key_status_after_restart'] = loss.forwarded('create_status', key) - status_before_restart
            case['native_creates_after_restart'] = loss.count('create_managed') - creates_before
            case['starts_after_restart'] = records(lab.state / 'provider-starts.jsonl')[len(starts):]
            case['effects_after_restart'] = records(lab.state / 'effects.jsonl')[len(effects):]
            case['runtimes_after_restart'] = sorted(inventory(lab))
            mark(case, same_key=case['same_key_status_before_restart'] >= 1
                 and case['same_key_status_after_restart'] >= 1
                 and case['pending'] == case['pending_after_restart']
                 and case['catalog_after_restart'] == case['catalog']
                 and first == case['status_after_restart'],
                 no_retry=single_create and case['native_creates_after_restart'] == 1
                 and case['effects_after_restart'] == case['effects'],
                 no_process=not case['starts_after_restart']
                 and case['runtimes_after_restart'] == sorted(before))


def catalog_fault(report, lab, legacy_ids, app_args, action):
    with scenario(report, 'R2-' + action) as case:
        db = lab.catalog / CATALOG_DB
        # @invariant: Rename within the catalog directory to preserve the original database inode.
        saved = db.with_name('saved-' + db.name)
        require(not os.path.lexists(saved), 'Earlier catalog fault left an unrestored database')
        before = legacy_snapshot(lab, legacy_ids)
        case['legacy_before'] = before
        case['tmux_before'] = generation(lab)
        case['healthy_catalog'] = catalog_snapshot(lab)
        db.rename(saved)
        try:
            if action == 'corrupt':
                db.write_bytes(b'disposable corrupt catalog fixture\n')
            else:
                shutil.copy2(saved, db)
                with closing(sqlite3.connect(db)) as connection:
                    connection.execute('PRAGMA user_version = 2147483647')
                    case['fixture_schema'] = connection.execute('PRAGMA user_version').fetchone()[0]
            db.chmod(0o600)
            case['fault_digest_before'] = digest(db)
            provision = lab.catalog / 'catalog.provision'
            case['provision_digest_before'] = digest(provision)
            capture_restart(case, lab)
            expected = 'catalog_unavailable' if action == 'corrupt' else 'catalog_unsupported'
            status_key = next(iter(case['healthy_catalog']['operations']))
            status = response_state(product(lab, 'create_status', key=status_key))
            blocked = product(lab, 'register_workspace', locator=str(lab.state))['product']
            case.update(key=status_key, create_status=status, registration=blocked,
                        fault_startup=catalog_message(lab.state))
            with native(case, *app_args):
                observe(case, 'native_error', f'Choose {lab.state} in the native folder picker. '
                        'Does the dialog report a catalog error while '
                        'the live legacy terminal still opens and the exited legacy entry remains visible?')
            case['fault_digest_after'] = digest(db)
            case['provision_digest_after'] = digest(provision)
            case['legacy_after'] = legacy_snapshot(lab, legacy_ids)
            case['tmux_after'] = generation(lab)
            mark(case, blocked=blocked == {'result': 'blocked', 'issue': expected}
                 and status == {'state': 'unknown', 'issue': expected},
                 catalog_unchanged=case['fault_digest_before'] == case['fault_digest_after']
                 and case['provision_digest_before'] == case['provision_digest_after'],
                 legacy_survives=case['legacy_after'] == before and case['tmux_before'] == case['tmux_after'])
        finally:
            saved.replace(db)
            capture_restart(case, lab)
            case['restored_catalog'] = catalog_snapshot(lab)
            require(case['healthy_catalog'] == case['restored_catalog'], 'Restored catalog identity changed')


def picker_traffic(before, after):
    deltas = {op: after['requests'][op] - before['requests'][op] for op in CANCEL_OPERATIONS}
    connected = 0 < before['forwarded_lists'] < after['forwarded_lists']
    intact = (before['pass_through'] and after['pass_through'] and not after['active']
              and before['failures'] == after['failures'] == 0)
    return {'connected': connected, 'intact': intact, 'deltas': deltas,
            'before': before, 'after': after}


def cancel_picker(report, lab):
    with scenario(report, 'R3-picker-cancel') as case:
        client = lab.state / ('cancel-' + uuid4().hex[:8])
        client.mkdir(mode=0o700)
        (client / 'tmux.sock').symlink_to(lab.state / 'tmux.sock')
        loss = ResponseLoss()
        with agent_proxy(lab, client, loss):
            binary = app_binary()
            app = subprocess.Popen([str(binary)], stdin=subprocess.DEVNULL,
                                   env=dict(os.environ, RS_STATE_DIR=str(client), RS_TMUX_BIN=lab.tmux))
            try:
                try:
                    wait_for(lambda: loss.forwarded('list'), timeout=8)
                except TimeoutError:
                    case['recorder'] = {'connected': False, 'before': loss.cancel_snapshot()}
                    mark(case, no_requests=False)
                    raise
                input('Open the folder picker in the disposable app. Leave it open and press Enter. ')
                pending_before = pending_records(client)
                catalog_before = catalog_snapshot(lab)
                panes_before = inventory(lab)
                traffic_before = loss.cancel_snapshot()
                observe(case, 'cancelled', 'Cancel the folder picker. '
                        'Did Create stay disabled without adding a tab?')
                traffic_decision = loss.cancel_snapshot()
                require(app.poll() is None, 'Picker app exited during cancellation')
                try:
                    wait_for(lambda: loss.forwarded('list') > traffic_decision['forwarded_lists'], timeout=8)
                    post_decision_connection = True
                except TimeoutError:
                    post_decision_connection = False
            finally:
                if app.poll() is None:
                    app.terminate()
                try:
                    app.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    app.kill()
                    app.wait(timeout=5)
        traffic = case['recorder'] = picker_traffic(traffic_before, loss.cancel_snapshot())
        traffic['at_decision'] = traffic_decision
        traffic['connected'] = traffic['connected'] and post_decision_connection
        pending_after = pending_records(client)
        panes_after = inventory(lab)
        catalog_after = catalog_snapshot(lab)
        case['pending_before'] = sorted(pending_before)
        case['pending_after'] = sorted(pending_after)
        case['runtime_ids_before'] = sorted(panes_before)
        case['runtime_ids_after'] = sorted(panes_after)
        case['workspaces_before'] = catalog_before['workspace_ids']
        case['workspaces_after'] = catalog_after['workspace_ids']
        mark(case, no_requests=traffic['connected'] and traffic['intact'] and not any(traffic['deltas'].values()),
             no_effect=pending_before == pending_after and catalog_before == catalog_after
             and panes_before == panes_after)


def real_profile(report, lab, name, folder, profile):
    with scenario(report, name) as case:
        if folder is None:
            case['reason'] = 'Owner-approved KINGSTON workspace was unavailable'
            return
        before = pending_records(lab.state)
        catalog_before = catalog_snapshot(lab)
        runtimes_before = inventory(lab)
        display = {'claude_code': 'Claude Code', 'codex': 'Codex'}[profile]
        with native(case):
            select_workspace(case, folder, display)
            if case['checks']['picker'] is not True:
                return
            key = submit_native(case, lab.state, before)
            observe(case, 'provider_started', f'Did the real {display} startup UI or its normal login/trust '
                    'screen render in the terminal? Handle its own trust choices and leave it open.')
            observe(case, 'one_tab', 'Is exactly one tab selected for this new runtime?')
            observe(case, 'no_prompts', 'Did you submit zero model prompts?')
            case['status_reply'] = state = response_state(product(lab, 'create_status', key=key))
            require(state.get('state') == 'confirmed', 'Real provider create was not confirmed')
            runtime = state['outcome']['runtime']
            after = inventory(lab)
            case['runtime'] = observed = checked_runtime(lab, after[runtime['runtime_id']])
            case['catalog'] = catalog = catalog_snapshot(lab)
            op = catalog['operations'][key]
            case['service_readiness'] = {
                choice: product(lab, 'workspace_readiness', workspace_id=op['workspace_id'],
                                profile=choice)['product']
                for choice in ('claude_code', 'codex')
            }
            command = [op['executable'], '--version']
            version = run(*command, check=False)
            case['commands'].append({'argv': command, 'exit': version.returncode})
            match = re.search(r'\b[0-9]+\.[0-9]+\.[0-9]+(?:[-+][A-Za-z0-9.\-]+)?\b', version.stdout)
            case['provider_version'] = match[0] if match and version.returncode == 0 else None
            case['new_runtimes'] = sorted(after.keys() - runtimes_before.keys())
            case['new_requests'] = sorted(pending_records(lab.state).keys() - before.keys())
            case['new_catalog_keys'] = sorted(catalog['operations'].keys() - catalog_before['operations'].keys())
            mark(case, service_ready=all(state == {'result': 'ready'} for state in case['service_readiness'].values())
                 and case['provider_version'] is not None,
                 one_launch=case['new_requests'] == case['new_catalog_keys'] == [key]
                 and case['new_runtimes'] == [op['runtime_id']]
                 and op['profile'] == profile and op['stage'] == 'finished' and state['current'] == 'live'
                 and op['operation_id'] == observed['operation_id'] and op['session_id'] == observed['session_id']
                 and op['target'] == observed['tmux_name'] and observed['origin'] == 'managed',
                 exact_cwd=op['workspace']['canonical'] == str(folder) == observed['pane']['cwd']
                 == observed['pane']['start_cwd']
                 and op['workspace']['inode'] == case['selected_folder']['inode']
                 and op['workspace']['device'] == case['selected_folder']['device'])


def workspace_on_kingston(path):
    mount = Path('/Volumes/KINGSTON')
    folder = Path(path).resolve(strict=True)
    if (not folder.is_dir() or mount not in folder.parents
            or folder.stat().st_dev != mount.stat().st_dev):
        raise ValueError('Choose an owner-approved workspace directory on /Volumes/KINGSTON')
    return folder


def prepare_kingston_workspace(report, path):
    root = workspace_on_kingston(path)
    folder = root / ('KINGSTON Проба ' + uuid4().hex)
    folder.mkdir(mode=0o700)
    report['kingston'] = path_identity(folder)
    return folder


def run_recovery(report, kingston):
    require(sys.platform == 'darwin' and sys.stdin.isatty(), 'Use a logged-in Mac Terminal for this procedure')
    lab = W1Record(W1_MANIFEST)
    binary = app_binary()
    report['build_instructions'] = [
        ['make', 'build-runtime'], ['npm', 'run', 'tauri', '--', 'build', '--debug', '--bundles', 'app'],
        app_command('up', '--w1'), app_command('down', '--w1'),
    ]
    report['source'] = {
        'revision': run('git', '-C', ROOT, 'rev-parse', 'HEAD').stdout.strip(),
        'tracked_changes': [line for line in
                            run('git', '-C', ROOT, 'status', '--short', '--untracked-files=no').stdout.splitlines()],
        'app_sha256': digest(binary), 'agent_sha256': digest(lab.host),
        'runner_sha256': {name: digest(ROOT / name) for name in
                          ('scripts/runtime_lab.py', 'scripts/desktop_lab.py',
                           'tests/integration/w1_recovery.py')},
        'os': run('sw_vers', '-productVersion').stdout.strip(),
        'tmux': run(lab.tmux, '-V').stdout.strip(),
        'python': sys.version.split()[0], 'observer_sqlite': sqlite3.sqlite_version,
    }
    report['desktop_lab'] = lab.record
    report['loaded_jobs'] = {}
    for name, target in lab.targets.items():
        pid = lab.loaded_pid(name)
        require(pid, 'W1 job is not running: ' + target)
        report['loaded_jobs'][target] = {'pid': pid, 'started': process_start(pid)}
    report['catalog'] = catalog_snapshot(lab)
    report['catalog']['internal_data_device'] = internal_device()
    report['catalog']['volume'] = run('df', '-P', lab.catalog).stdout.splitlines()[-1]
    print('Use the indicated app in each case. Quit other Remote Sessions windows first. '
          'Follow normal provider login and trust prompts and submit zero model prompts.')
    if kingston is None:
        kingston = input('Enter an owner-approved KINGSTON parent for a new workspace, or Enter to skip. ').strip()
    if kingston:
        try:
            kingston = prepare_kingston_workspace(report, kingston)
        except (OSError, ValueError) as error:
            report['kingston'] = {'issue': str(error)}
            kingston = None
    else:
        kingston = None

    cancel_picker(report, lab)
    internal = lab.state / ('internal Проба ' + uuid4().hex[:8])
    internal.mkdir(mode=0o700)
    require(internal.stat().st_dev == report['catalog']['internal_data_device'],
            'The internal workspace is outside the data volume')
    real_profile(report, lab, 'R3-internal-claude', internal, 'claude_code')
    real_profile(report, lab, 'R3-kingston-codex', kingston, 'codex')

    with fixture_jobs() as fixture:
        report['fixture_jobs'] = fixture.record
        empty_quick_exit(report, fixture)
        legacy_ids, before_legacy_exit = add_legacy(fixture)
        name = 'client-' + uuid4().hex[:12]
        client = fixture.state / name
        client.mkdir(mode=0o700)
        (client / 'tmux.sock').symlink_to(fixture.state / 'tmux.sock')
        loss = ResponseLoss()
        with agent_proxy(fixture, client, loss):
            fixture_client_state(fixture, name)
            preflight = attach_preflight(fixture, client, legacy_ids[0])
            app_args = ('--fixture', '--client-state', name)
            legacy_case(report, fixture, legacy_ids, before_legacy_exit, app_args)
            try:
                lost_reply_case(report, fixture, client, loss, preflight, app_args)
            finally:
                loss.disarm()
            for name, action in [('R3-no-effect', 'definite'), ('R3-late-replacement', 'replacement'),
                                 ('R1-unknown', 'unknown')]:
                try:
                    fault_launch(report, fixture, client, loss, app_args, name, action)
                finally:
                    (fixture.state / 'effect-barrier.json').unlink(missing_ok=True)
            for action in ('corrupt', 'newer'):
                catalog_fault(report, fixture, legacy_ids, app_args, action)
    report['fixture_teardown'] = 'Disposable fixture jobs unloaded and both directories removed.'
    report['desktop_teardown'] = 'Operator command recorded under build_instructions.'


def report_path(value):
    path = value.absolute()
    path = path.parent.resolve(strict=True) / path.name
    if path.name.startswith('P0-'):
        raise ValueError('Select a separate W1 report outside historical reports')
    return path


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--report', type=Path, required=True, help='New private JSON report path')
    parser.add_argument('--kingston', type=Path, help='Owner-approved parent for a new workspace on /Volumes/KINGSTON')
    args = parser.parse_args()
    try:
        path = report_path(args.report)
        descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    except (OSError, ValueError) as error:
        parser.error(str(error))
    report = {
        'format': 'w1-native-recovery-1', 'started_at': datetime.now(timezone.utc).isoformat(),
        'validation_tasks': {'01-03-01': 'desktop_lab', '01-03-02': 'cases'},
        'cases': {name: {'status': Verdict.UNRUN, 'checks': {}, 'commands': []} for name in CASE_CHECKS},
    }
    with os.fdopen(descriptor, 'w') as output:
        try:
            run_recovery(report, args.kingston)
        except (KeyboardInterrupt, EOFError):
            report['interrupted'] = True
        except (AssertionError, OSError, LookupError, TypeError, ValueError, RuntimeError,
                sqlite3.Error, subprocess.SubprocessError) as error:
            report['error'] = str(error)
            print(error, file=sys.stderr)
        finally:
            report['finished_at'] = datetime.now(timezone.utc).isoformat()
            report['complete'] = ('error' not in report and 'interrupted' not in report
                                  and 'fixture_teardown' in report
                                  and all(case['status'] == Verdict.PASS for case in report['cases'].values()))
            json.dump(report, output, indent=2, ensure_ascii=False)
            output.write('\n')
    print(f'W1 recovery report written to {path}')
    return 0 if report['complete'] else 1


if __name__ == '__main__':
    sys.exit(main())
