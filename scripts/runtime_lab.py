#!/usr/bin/env python3
"""Disposable, independent launchd services for the local desktop labs (macOS only)."""
import argparse
import json
import os
from pathlib import Path
import plistlib
import pwd
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / '.cache/runtime-lab.json'
W1_MANIFEST = ROOT / '.cache/runtime-lab-w1.json'
W1_LABEL = 'dev.remotesessions.w1.'
W1_ID = re.compile(r'[0-9a-f]{12}')
CATALOG_DB = 'catalog.sqlite3'


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


def support_directory():
    return Path(pwd.getpwuid(os.getuid()).pw_dir) / 'Library/Application Support'


def internal_device():
    return Path('/System/Volumes/Data').stat().st_dev


def w1_paths(lab_id):
    if not isinstance(lab_id, str) or not W1_ID.fullmatch(lab_id):
        raise ValueError('Invalid W1 lab identity')
    return (Path('/tmp').resolve() / ('rs-w1-' + lab_id),
            support_directory() / (W1_LABEL + lab_id))


def private_path(path, kind, mode):
    info = path.lstat()
    modes = mode if isinstance(mode, tuple) else (mode,)
    if (not kind(info.st_mode) or stat.S_IMODE(info.st_mode) not in modes
            or info.st_uid != os.getuid() or path.resolve(strict=True) != path
            or (stat.S_ISREG(info.st_mode) and info.st_nlink != 1)):
        raise ValueError(f'Unexpected private lab path: {path}')
    return info


def directory_identity(path):
    info = private_path(path, stat.S_ISDIR, 0o700)
    return [info.st_dev, info.st_ino]


def write_w1_manifest(path, record):
    path.parent.mkdir(exist_ok=True)
    with tempfile.NamedTemporaryFile(mode='w', dir=path.parent) as output:
        json.dump(record, output, indent=2)
        output.write('\n')
        output.flush()
        # @invariant: Publish the complete private manifest without replacing an existing one.
        os.link(output.name, path)


def job_argv(state, catalog, tmux, agent_tmux):
    return {
        'tmux': [tmux, '-D', '-S', str(state / 'tmux.sock'), '-f', str(state / 'tmux.conf')],
        'agent': [str(state / 'host-agent'), '--state-dir', str(state),
                  '--tmux', agent_tmux, '--catalog-dir', str(catalog), 'serve'],
    }


class W1Record:
    """Resolve every job and deletion target from one disposable lab identity."""

    def __init__(self, manifest, *, fixture=False, stopping=False):
        self.manifest = manifest
        private_path(manifest, stat.S_ISREG, 0o600)
        self.record = data = json.loads(manifest.read_text())
        if not isinstance(data, dict) or data.get('fixture') is not fixture:
            raise ValueError('Unexpected W1 lab mode')
        lab_id = data.get('id')
        self.state, self.catalog = w1_paths(lab_id)
        self.domain = f'gui/{os.getuid()}'
        self.prefix = W1_LABEL + lab_id
        self.targets = {name: self.domain + '/' + self.prefix + '.' + name
                        for name in ('tmux', 'agent')}
        self.tmux = data.get('tmux')
        self.host = self.state / 'host-agent'
        self.device = internal_device()
        if (data.get('state_dir') != str(self.state) or data.get('catalog_dir') != str(self.catalog)
                or data.get('targets') != list(self.targets.values())
                or not isinstance(self.tmux, str) or not Path(self.tmux).is_absolute()
                or str(self.state / 'tmux-fixture') == self.tmux):
            raise ValueError('Unexpected W1 paths or service targets')
        agent_tmux = str(self.state / 'tmux-fixture') if fixture else self.tmux
        self.argv = job_argv(self.state, self.catalog, self.tmux, agent_tmux)
        if data.get('startup_argv') != self.argv:
            raise ValueError('Unexpected W1 startup arguments')
        self.validate_directories(stopping=stopping)
        if self.state.exists():
            for name in self.targets:
                self.validate_job_definition(name)

    def validate_job_definition(self, name):
        path = self.state / (name + '.plist')
        private_path(path, stat.S_ISREG, 0o600)
        definition = plistlib.loads(path.read_bytes())
        if (definition.get('Label') != self.prefix + '.' + name
                or definition.get('ProgramArguments') != self.argv[name]
                or definition.get('WorkingDirectory') != str(self.state)):
            raise ValueError('Unexpected W1 service definition')

    def validate_directories(self, *, stopping=False):
        for name, path in (('state', self.state), ('catalog', self.catalog)):
            try:
                identity = directory_identity(path)
            except FileNotFoundError:
                if stopping:
                    continue
                raise
            if identity != self.record.get(name + '_identity'):
                raise ValueError(f'W1 {name} directory identity changed')
            if name == 'catalog' and identity[0] != self.device:
                raise ValueError('W1 catalog is outside the internal data volume')

    def loaded_pid(self, name, *, allow_missing=False):
        target = self.targets[name]
        result = run('launchctl', 'print', target, check=False)
        if result.returncode:
            if allow_missing and ('Could not find service' in result.stderr
                                  or 'Could not find specified service' in result.stderr):
                return None
            raise ProcessFailure(result.returncode, result.args, result.stdout, result.stderr)
        fields = [line.strip() for line in result.stdout.splitlines()]
        if (not self.state.exists()
                or f'path = {self.state / (name + ".plist")}' not in fields):
            raise ValueError('Loaded W1 service has an unexpected launchd definition: ' + target)
        self.validate_job_definition(name)
        pids = [int(line[6:]) for line in fields if re.fullmatch(r'pid = [0-9]+', line)]
        if len(pids) > 1:
            raise ValueError('Unexpected launchd PID output')
        return pids[0] if pids else 0

    def restart_agent(self):
        self.validate_directories()
        before = self.loaded_pid('agent')
        if not before:
            raise RuntimeError('W1 agent is not running')
        run('launchctl', 'kickstart', '-k', self.targets['agent'])

        def restarted():
            pid = self.loaded_pid('agent')
            return pid if pid and pid != before and self.call('health')['agent_pid'] == pid else None

        return [before, wait_for(restarted)]

    def call(self, *args):
        return json.loads(run(self.host, '--state-dir', self.state, '--tmux', self.tmux, *args).stdout)

    def tmux_call(self, *args):
        return run(self.tmux, '-N', '-S', self.state / 'tmux.sock', *args, check=False)

    def close(self):
        self.validate_directories(stopping=True)
        loaded = {name: self.loaded_pid(name, allow_missing=True) for name in self.targets}
        for name in ('agent', 'tmux'):
            if loaded[name] is not None:
                run('launchctl', 'bootout', self.targets[name])
        deadline = time.monotonic() + 15
        while True:
            remaining = [name for name in self.targets
                         if self.loaded_pid(name, allow_missing=True) is not None]
            if not remaining:
                break
            delay = deadline - time.monotonic()
            if delay <= 0:
                raise TimeoutError('W1 jobs are still loaded: ' + ', '.join(remaining))
            time.sleep(min(0.1, delay))
        self.validate_directories(stopping=True)
        for path in (self.state, self.catalog):
            if path.exists():
                shutil.rmtree(path)
        self.manifest.unlink()


class Lab:
    def __init__(self, *, w1=False, fixture=False):
        if sys.platform != 'darwin':
            raise RuntimeError('launchd lifecycle proof requires macOS')
        if fixture and not w1:
            raise ValueError('Recovery fixtures require W1 jobs')
        self.host = ROOT / 'target/debug/host-agent'
        self.tmux = shutil.which('tmux')
        if not self.host.is_file() or not self.tmux:
            raise RuntimeError('build host-agent and install tmux first')
        self.w1 = w1
        self.fixture = fixture
        self.id = uuid.uuid4().hex[:12]
        self.catalog = None
        self.catalog_created = False
        self.domain = f'gui/{os.getuid()}'
        self.prefix = (W1_LABEL if w1 else 'dev.remotesessions.p0.') + self.id
        self.loaded = []
        if w1:
            self.state, self.catalog = w1_paths(self.id)
            if (self.catalog.parent.resolve(strict=True) != self.catalog.parent
                    or self.catalog.parent.stat().st_dev != internal_device()):
                raise RuntimeError('W1 requires a real Application Support directory on the internal data volume')
            self.tmux = str(Path(self.tmux).resolve(strict=True))
            self.state.mkdir(mode=0o700)
        else:
            self.state = Path(tempfile.mkdtemp(prefix='rs-p0-', dir='/tmp')).resolve()
        self.service_host = self.state / 'host-agent'
        try:
            shutil.copy2(self.host, self.service_host)
            shutil.copy2(ROOT / 'config/tmux.conf', self.state / 'tmux.conf')
        except Exception:
            self.close()
            raise
        self.agent_path = os.environ['PATH']
        self.startup_argv = {
            'tmux': [self.tmux, '-D', '-S', str(self.state / 'tmux.sock'), '-f', str(self.state / 'tmux.conf')],
            'agent': [str(self.service_host), '--state-dir', str(self.state), '--tmux', self.tmux, 'serve'],
        }

    def w1_record(self):
        return {
            'id': self.id, 'fixture': self.fixture, 'state_dir': str(self.state),
            'catalog_dir': str(self.catalog), 'tmux': self.tmux, 'targets': self.loaded,
            'startup_argv': self.startup_argv, 'state_identity': directory_identity(self.state),
            'catalog_identity': directory_identity(self.catalog),
            'catalog_provision_argv': [str(self.service_host), '--catalog-dir', str(self.catalog), 'catalog-init'],
            'catalog_startup': catalog_message(self.state),
        }

    def start(self):
        try:
            if self.w1:
                self.catalog.mkdir(mode=0o700)
                self.catalog_created = True
                run(self.service_host, '--catalog-dir', self.catalog, 'catalog-init')
                agent_tmux = str(self.state / 'tmux-fixture') if self.fixture else self.tmux
                self.startup_argv = job_argv(self.state, self.catalog, self.tmux, agent_tmux)
            for name, args in self.startup_argv.items():
                label = self.prefix + '.' + name
                path = self.state / (name + '.plist')
                definition = {'Label': label, 'ProgramArguments': args, 'WorkingDirectory': str(self.state),
                              'KeepAlive': True, 'ThrottleInterval': 1, 'ProcessType': 'Interactive',
                              'Umask': 63, 'EnvironmentVariables': {'PATH': self.agent_path if name == 'agent' else os.environ['PATH'], 'LANG': 'en_US.UTF-8'},
                              'StandardOutPath': str(self.state / (name + '.stdout.log')),
                              'StandardErrorPath': str(self.state / (name + '.stderr.log'))}
                path.write_bytes(plistlib.dumps(definition))
                path.chmod(0o600)
                run('launchctl', 'bootstrap', self.domain, path)
                self.loaded.append(self.domain + '/' + label)
            wait_for(lambda: self.call('health').get('agent_pid'))
            wait_for(lambda: self.tmux_call('show-options', '-s', 'exit-empty').returncode == 0)
            if self.w1 and not wait_for(lambda: catalog_message(self.state)).startswith('local catalog ready ('):
                raise RuntimeError('W1 catalog failed to open')
            return self
        except (Exception, KeyboardInterrupt):
            try:
                for target in self.loaded:
                    print("\n".join(line for line in run("launchctl", "print", target, check=False).stdout.splitlines() if any(key in line for key in ["state =", "pid =", "last exit", "program ="])), file=sys.stderr)
                for file in self.state.glob('*.log'):
                    print(file.name + ': ' + file.read_text(), file=sys.stderr)
            finally:
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
        if self.catalog_created:
            shutil.rmtree(self.catalog)

    def __enter__(self):
        return self.start()

    def __exit__(self, *_):
        self.close()


def catalog_message(state, offset=0):
    with (state / 'agent.stderr.log').open('rb') as log:
        log.seek(offset)
        lines = log.read().decode(errors='replace').splitlines()
    return next((line for line in reversed(lines)
                 if line.startswith(('local catalog ready (', 'local catalog unavailable ('))), '')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['up', 'down'])
    parser.add_argument('--w1', action='store_true')
    args = parser.parse_args()
    if args.w1:
        if args.action == 'down':
            W1Record(W1_MANIFEST, stopping=True).close()
            print('W1 runtime lab stopped.')
        else:
            if os.path.lexists(W1_MANIFEST):
                raise SystemExit('W1 lab manifest exists; run down --w1 before starting another lab')
            lab = Lab(w1=True).start()
            try:
                record = lab.w1_record()
                write_w1_manifest(W1_MANIFEST, record)
            except Exception:
                lab.close()
                raise
            print(json.dumps(record, indent=2))
        return
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
