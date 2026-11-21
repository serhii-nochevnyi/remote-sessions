#!/usr/bin/env python3
"""Exercise real launchd services, tmux, PTYs and process continuity on macOS."""
import argparse
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import signal
import shutil
import struct
import subprocess
import sys
import termios
import time
import uuid

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'scripts'))
from runtime_lab import Lab, run, wait_for


def frame(state, message):
    import socket
    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(7)
        stream.connect(str(state / 'agent.sock'))
        stream.sendall(message)
        response = bytearray()
        while not response.endswith(b'\n'):
            part = stream.recv(65536)
            if not part:
                raise AssertionError('agent closed without a response')
            response.extend(part)
        return json.loads(response)


class Viewer:
    def __init__(self, lab, runtime_id, width=80, height=24):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.environ['TERM'] = 'xterm-256color'
            os.execv(str(lab.host), [str(lab.host), '--state-dir', str(lab.state), '--tmux', lab.tmux, 'attach', runtime_id])
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack('HHHH', height, width, 0, 0))
        os.set_blocking(self.fd, False)
        self.output = bytearray()

    def read(self):
        while select.select([self.fd], [], [], 0)[0]:
            try:
                data = os.read(self.fd, 65536)
            except (BlockingIOError, OSError):
                break
            if not data:
                break
            self.output.extend(data)
        return b'P0 runtime alive' in self.output

    def close(self, abrupt=False):
        if self.fd is None:
            return
        if abrupt:
            os.kill(self.pid, signal.SIGKILL)
        else:
            os.write(self.fd, b'\x02d')
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            self.read()
            ended, status = os.waitpid(self.pid, os.WNOHANG)
            if ended:
                if not abrupt:
                    assert os.waitstatus_to_exitcode(status) == 0, "viewer did not detach cleanly"
                break
            time.sleep(0.05)
        else:
            os.kill(self.pid, signal.SIGKILL)
            os.waitpid(self.pid, 0)
            raise AssertionError('viewer failed to detach')
        os.close(self.fd)
        self.fd = None


def verify():
    evidence = {'platform': sys.platform, 'tmux': run('tmux', '-V').stdout.strip(), 'checks': []}
    viewers = []
    with Lab() as lab:
        try:
            def check(name):
                evidence['checks'].append(name)
                print('PASS ' + name, flush=True)

            assert lab.call('list')['runtimes'] == []
            assert (lab.state / 'agent.sock').stat().st_mode & 0o777 == 0o600
            duplicate = run(lab.host, '--state-dir', lab.state, '--tmux', lab.tmux, 'serve', check=False)
            assert duplicate.returncode != 0 and 'another agent' in duplicate.stderr
            assert frame(lab.state, b'{"op":"send_input"}\n')['result'] == 'error'
            assert frame(lab.state, b'x' * 65537 + b'\n')['result'] == 'error'
            check('private IPC, single-agent lock and bounded/unknown-request rejection')

            probe = lab.state / 'runtime_probe.py'
            shutil.copy2(ROOT / 'tests/integration/runtime_probe.py', probe)
            session_id, runtime_id = str(uuid.uuid4()), str(uuid.uuid4())
            heartbeat, received = lab.state / 'heartbeat.json', lab.state / 'input.txt'
            created = lab.call('create', '--session-id', session_id, '--runtime-id', runtime_id,
                               '--cwd', lab.state, '--', sys.executable, probe, heartbeat, received)['runtime']
            before = wait_for(lambda: json.loads(heartbeat.read_text()))
            assert created['pane_pid'] == before['pid']
            start_time = run('ps', '-p', before['pid'], '-o', 'lstart=').stdout.strip()
            tmux_pid = lab.tmux_call('display-message', '-p', '#{pid}').stdout.strip()
            evidence['identity'] = {'session_id': session_id, 'runtime_id': runtime_id, 'pane_id': created['pane_id'], 'pane_pid': before['pid'], 'process_started': start_time, 'instance': before['instance'], 'tmux_pid': int(tmux_pid)}
            assert lab.call('create', '--session-id', session_id, '--runtime-id', runtime_id, '--cwd', lab.state, '--', '/bin/sh', '-i', check=False).returncode != 0
            check('real process created through agent; duplicate create rejected')

            def continuity(previous):
                now = wait_for(lambda: (state if (state := json.loads(heartbeat.read_text()))['sequence'] > previous['sequence'] else None))
                assert now['pid'] == before['pid'] and now['instance'] == before['instance']
                assert run('ps', '-p', now['pid'], '-o', 'lstart=').stdout.strip() == start_time
                runtimes = lab.call('list')['runtimes']
                current = next(r for r in runtimes if r['runtime_id'] == runtime_id)
                for key in ['session_id', 'runtime_id', 'pane_id', 'pane_pid', 'width', 'height']:
                    assert current[key] == created[key], (key, current, created)
                assert not current['exited']
                assert lab.tmux_call('display-message', '-p', '#{pid}').stdout.strip() == tmux_pid
                return now

            first = Viewer(lab, runtime_id); viewers.append(first)
            wait_for(first.read)
            os.write(first.fd, b'INPUT_MUST_NOT_REACH_PROCESS\n')
            second = Viewer(lab, runtime_id, width=41, height=12); viewers.append(second)
            wait_for(second.read)
            after = continuity(before)
            assert not received.exists()
            check('two ordinary read-only PTY attachments render live output without input/resize authority')
            first.close()
            after = continuity(after)
            second.close(abrupt=True)
            after = continuity(after)
            check('normal detach and SIGKILL client loss preserve process identity and progress')

            agent_target = lab.domain + '/' + lab.prefix + '.agent'
            old_pid = lab.call('health')['agent_pid']
            run('launchctl', 'kill', 'SIGTERM', agent_target)
            new_pid = wait_for(lambda: (pid if (pid := lab.call('health')['agent_pid']) != old_pid else None))
            after = continuity(after)
            check('SIGTERM agent shutdown and launchd restart preserve tmux/runtime identity')
            run('launchctl', 'kickstart', '-k', agent_target)
            killed_pid = wait_for(lambda: (pid if (pid := lab.call('health')['agent_pid']) != new_pid else None))
            after = continuity(after)
            evidence['agent_pids'] = [old_pid, new_pid, killed_pid]
            check('forced launchctl kickstart -k recovers stale IPC and inventory without respawning runtime')

            resumed = Viewer(lab, runtime_id); viewers.append(resumed)
            wait_for(resumed.read)
            after = continuity(after)
            assert 'Робота триває' in lab.call('snapshot', runtime_id)['text']
            resumed.close()
            evidence['progress'] = {'before': before['sequence'], 'after': after['sequence']}
            check('fresh PTY reattachment after service restart displays current output')

            other = lab.call('create', '--cwd', lab.state, '--', '/bin/sh', '-i')['runtime']
            invalid = lab.call('stop', runtime_id, '--expected-pane-id', '%999999999', check=False)
            assert invalid.returncode != 0
            continuity(after)
            lab.call('stop', runtime_id, '--expected-pane-id', created['pane_id'])
            wait_for(lambda: run('ps', '-p', before['pid'], check=False).returncode != 0)
            remaining = lab.call('list')['runtimes']
            assert len(remaining) == 1 and remaining[0]['runtime_id'] == other['runtime_id'] and not remaining[0]['exited']
            lab.call('stop', other['runtime_id'], '--expected-pane-id', other['pane_id'])
            check('stop precondition rejects stale target; explicit stop leaves unrelated runtime alive')

            ended = lab.call('create', '--cwd', lab.state, '--', '/bin/sh', '-c', 'exit 7')['runtime']
            wait_for(lambda: next(r for r in lab.call('list')['runtimes'] if r['runtime_id'] == ended['runtime_id'])['exited'])
            assert lab.call('attach', ended['runtime_id'], check=False).returncode != 0
            lab.call('stop', ended['runtime_id'], '--expected-pane-id', ended['pane_id'])
            check('natural CLI exit is retained as exited and never resurrected')

            run('launchctl', 'bootout', lab.domain + '/' + lab.prefix + '.tmux')
            lab.loaded.remove(lab.domain + '/' + lab.prefix + '.tmux')
            failed = lab.call('create', '--cwd', lab.state, '--', '/bin/sh', '-i', check=False)
            assert failed.returncode != 0
            assert lab.tmux_call('list-sessions').returncode != 0
            check('missing tmux service fails closed; agent never starts its own replacement')
        finally:
            for viewer in viewers:
                if viewer.fd is not None:
                    viewer.close(abrupt=True)
    evidence['cleanup'] = 'Both temporary launchd jobs unloaded; private test directory removed.'
    return evidence


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--report', type=Path)
    args = parser.parse_args()
    evidence = verify()
    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(evidence, indent=2, ensure_ascii=False) + '\n')
    print(json.dumps(evidence, indent=2, ensure_ascii=False))
