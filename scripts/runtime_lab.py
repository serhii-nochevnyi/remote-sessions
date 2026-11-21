#!/usr/bin/env python3
"""Disposable, independent launchd services for the local P0 spike (macOS only)."""
import argparse
import json
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import sys
import tempfile
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / '.cache/runtime-lab.json'


class ProcessFailure(subprocess.CalledProcessError):
    def __str__(self):
        return super().__str__() + "\n" + (self.stderr or "")[:2000]


def run(*args, check=True):
    result = subprocess.run([str(arg) for arg in args], capture_output=True, text=True, timeout=15)
    if check and result.returncode:
        raise ProcessFailure(result.returncode, result.args, result.stdout, result.stderr)
    return result


def wait_for(check, timeout=15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            value = check()
            if value:
                return value
        except (OSError, ValueError, subprocess.SubprocessError):
            pass
        time.sleep(0.1)
    raise TimeoutError('condition did not become true within timeout')


class Lab:
    def __init__(self):
        if sys.platform != 'darwin':
            raise RuntimeError('launchd lifecycle proof requires macOS')
        self.host = ROOT / 'target/debug/host-agent'
        self.tmux = shutil.which('tmux')
        if not self.host.is_file() or not self.tmux:
            raise RuntimeError('build host-agent and install tmux first')
        self.state = Path(tempfile.mkdtemp(prefix='rs-p0-', dir='/tmp')).resolve()
        # Install a disposable service payload, independent of checkout location.
        self.service_host = self.state / 'host-agent'
        shutil.copy2(self.host, self.service_host)
        shutil.copy2(ROOT / 'config/tmux.conf', self.state / 'tmux.conf')
        self.domain = f'gui/{os.getuid()}'
        self.prefix = 'dev.remotesessions.p0.' + uuid.uuid4().hex[:12]
        self.loaded = []

    def start(self):
        try:
            for name, args in [
                ('tmux', [self.tmux, '-D', '-S', str(self.state / 'tmux.sock'), '-f', str(self.state / 'tmux.conf')]),
                ('agent', [str(self.service_host), '--state-dir', str(self.state), '--tmux', self.tmux, 'serve']),
            ]:
                label = self.prefix + '.' + name
                path = self.state / (name + '.plist')
                definition = {'Label': label, 'ProgramArguments': args, 'WorkingDirectory': str(self.state),
                              'KeepAlive': True, 'ThrottleInterval': 1, 'ProcessType': 'Interactive',
                              'Umask': 63, 'EnvironmentVariables': {'PATH': os.environ['PATH'], 'LANG': 'en_US.UTF-8'},
                              'StandardOutPath': str(self.state / (name + '.stdout.log')),
                              'StandardErrorPath': str(self.state / (name + '.stderr.log'))}
                path.write_bytes(plistlib.dumps(definition))
                path.chmod(0o600)
                run('launchctl', 'bootstrap', self.domain, path)
                self.loaded.append(self.domain + '/' + label)
            wait_for(lambda: self.call('health').get('agent_pid'))
            wait_for(lambda: self.tmux_call('show-options', '-s', 'exit-empty').returncode == 0)
            return self
        except Exception:
            for target in self.loaded:
                print("\n".join(line for line in run("launchctl", "print", target, check=False).stdout.splitlines() if any(key in line for key in ["state =", "pid =", "last exit", "program ="])), file=sys.stderr)
            for file in self.state.glob('*.log'):
                print(file.name + ': ' + file.read_text(), file=sys.stderr)
            self.close()
            raise

    def call(self, *args, check=True):
        result = run(self.host, '--state-dir', self.state, '--tmux', self.tmux, *args, check=check)
        if not check:
            return result
        return json.loads(result.stdout)

    def tmux_call(self, *args):
        return run(self.tmux, '-N', '-S', self.state / 'tmux.sock', *args, check=False)

    def close(self):
        failures = []
        for target in reversed(self.loaded):
            result = run('launchctl', 'bootout', target, check=False)
            if result.returncode and run('launchctl', 'print', target, check=False).returncode == 0:
                failures.append(target)
        if failures:
            raise RuntimeError('could not unload lab services: ' + ', '.join(failures))
        self.loaded.clear()
        shutil.rmtree(self.state)

    def __enter__(self):
        return self.start()

    def __exit__(self, *_):
        self.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['up', 'down'])
    args = parser.parse_args()
    if args.action == 'up':
        if MANIFEST.exists():
            raise SystemExit('lab manifest exists; run down before starting another lab')
        lab = Lab().start()
        MANIFEST.parent.mkdir(exist_ok=True)
        MANIFEST.write_text(json.dumps({'state_dir': str(lab.state), 'targets': lab.loaded}, indent=2) + '\n')
        MANIFEST.chmod(0o600)
        print(json.dumps({'state_dir': str(lab.state), 'targets': lab.loaded}, indent=2))
    else:
        record = json.loads(MANIFEST.read_text())
        # Only unload explicit lab service labels, never a whole launchd domain.
        for target in reversed(record['targets']):
            if not target.startswith(f'gui/{os.getuid()}/dev.remotesessions.p0.'):
                raise SystemExit('unexpected lab service target')
            result = run('launchctl', 'bootout', target, check=False)
            if result.returncode and run('launchctl', 'print', target, check=False).returncode == 0:
                raise SystemExit('could not unload ' + target)
        # Do not recursively delete a path supplied by a persisted manifest.
        MANIFEST.unlink()
        print('Lab services stopped. Temporary logs remain at ' + record['state_dir'])


if __name__ == '__main__':
    main()
