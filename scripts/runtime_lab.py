#!/usr/bin/env python3
"""Disposable, independent launchd services for the local desktop labs (macOS only)."""
import argparse
from contextlib import contextmanager
import fcntl
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
W1_JOBS = ('tmux', 'agent')
W1_TMUX_VERSION = '3.8'
CATALOG_DB = 'catalog.sqlite3'


class ProcessFailure(subprocess.CalledProcessError):
    def __str__(self):
        return super().__str__() + "\n" + (self.stderr or "")[:2000]


def run(*args, check=True, pass_fds=()):
    result = subprocess.run([str(arg) for arg in args], capture_output=True, text=True,
                            timeout=15, pass_fds=pass_fds)
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


def require_w1_tmux_version(probe, expected, source, remedy):
    try:
        result = probe()
    except (OSError, subprocess.SubprocessError, UnicodeError) as error:
        observed = f'unavailable ({type(error).__name__})'
    else:
        if result.returncode == 0:
            version = result.stdout.removesuffix('\n')
            if version == expected:
                return
            observed = repr(version[:160]) if version else 'unavailable (empty output)'
        else:
            observed = f'unavailable (exit {result.returncode})'
    raise RuntimeError(f'W1 requires {expected!r} from {source}. Observed {observed}. {remedy}')


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


@contextmanager
def w1_lock(path):
    path.parent.mkdir(exist_ok=True)
    fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
        yield fd
    finally:
        os.close(fd)


def sync_directory(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def private_output(path, mode):
    return os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), mode)


def staged_record(path):
    return path.with_suffix(path.suffix + '.tmp')


def w1_recovery_path(manifest):
    return manifest.with_suffix('.recovery.json')


def w1_pressure_manifest(lab_id):
    if not W1_ID.fullmatch(lab_id):
        raise ValueError('Invalid W1 lab identity')
    return W1_MANIFEST.with_name(f'runtime-lab-w1-pressure-{lab_id}.json')


def write_w1_manifest(path, record, *, replace=False):
    if os.path.lexists(path):
        # @invariant: Publish the complete private manifest without replacing an existing one.
        if not replace:
            raise FileExistsError(path)
        private_path(path, stat.S_ISREG, 0o600)
    pending = staged_record(path)
    with private_output(pending, 'w') as output:
        json.dump(record, output, indent=2)
        output.write('\n')
        output.flush()
        os.fsync(output.fileno())
    os.replace(pending, path)
    sync_directory(path.parent)


def job_argv(state, catalog, tmux, agent_tmux):
    return {
        'tmux': [tmux, '-D', '-S', str(state / 'tmux.sock'), '-f', str(state / 'tmux.conf')],
        'agent': [str(state / 'host-agent'), '--state-dir', str(state),
                  '--tmux', agent_tmux, '--catalog-dir', str(catalog), 'serve'],
    }


def job_definition(state, prefix, name, argv, path_env):
    return {'Label': prefix + '.' + name, 'ProgramArguments': argv, 'WorkingDirectory': str(state),
            'KeepAlive': True, 'ThrottleInterval': 1, 'ProcessType': 'Interactive', 'Umask': 63,
            'EnvironmentVariables': {'PATH': path_env, 'LANG': 'en_US.UTF-8'},
            'StandardOutPath': str(state / (name + '.stdout.log')),
            'StandardErrorPath': str(state / (name + '.stderr.log'))}


class W1Record:
    """Resolve every job and deletion target from one disposable lab identity."""

    def __init__(self, manifest, *, fixture=False, stopping=False, recovery=False):
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
                        for name in W1_JOBS}
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
        self.attempted = W1_JOBS
        self.definitions = None
        if recovery:
            self.definitions = definitions = data.get('attempted')
            complete = data.get('startup_complete')
            if (not isinstance(definitions, dict) or tuple(definitions) != W1_JOBS[:len(definitions)]
                    or type(complete) is not bool or (complete and tuple(definitions) != W1_JOBS)):
                raise ValueError('Unexpected W1 recovery state')
            self.attempted = tuple(definitions)
            for name, definition in definitions.items():
                environment = definition.get('EnvironmentVariables') if isinstance(definition, dict) else None
                path_env = environment.get('PATH') if isinstance(environment, dict) else None
                if (not isinstance(path_env, str)
                        or definition != job_definition(self.state, self.prefix, name, self.argv[name], path_env)):
                    raise ValueError('Unexpected W1 recovery definition')
        self.validate_directories(stopping=stopping)
        if not stopping:
            for name in self.attempted:
                self.validate_job_definition(name)

    def validate_job_definition(self, name):
        path = self.state / (name + '.plist')
        private_path(path, stat.S_ISREG, 0o600)
        definition = plistlib.loads(path.read_bytes())
        if (not isinstance(definition, dict) or definition.get('Label') != self.prefix + '.' + name
                or definition.get('ProgramArguments') != self.argv[name]
                or definition.get('WorkingDirectory') != str(self.state)
                or (self.definitions is not None and definition != self.definitions[name])):
            raise ValueError('Unexpected W1 service definition')

    def validate_directories(self, *, stopping=False):
        for name, path in (('state', self.state), ('catalog', self.catalog)):
            expected = self.record.get(name + '_identity')
            if (not isinstance(expected, list) or len(expected) != 2
                    or any(type(value) is not int for value in expected)):
                raise ValueError(f'Invalid W1 {name} directory identity')
            if name == 'catalog' and expected[0] != self.device:
                raise ValueError('W1 catalog is outside the internal data volume')
            try:
                identity = directory_identity(path)
            except FileNotFoundError:
                if stopping:
                    continue
                raise
            if identity != expected:
                raise ValueError(f'W1 {name} directory identity changed')

    def loaded_pid(self, name, *, allow_missing=False):
        target = self.targets[name]
        result = run('launchctl', 'print', target, check=False)
        if result.returncode:
            label = self.prefix + '.' + name
            absent = f'Could not find service "{label}" in domain for user gui: {os.getuid()}'
            diagnostics = result.stderr.strip().splitlines()
            if diagnostics[:1] == ['Bad request.']:
                diagnostics.pop(0)
            confirmed = (diagnostics == [absent] or (result.returncode == 113 and diagnostics in (
                ['Could not find service.'], ['Could not find specified service'])))
            if (allow_missing and result.returncode > 0 and not result.stdout.strip()
                    and confirmed):
                try:
                    self.validate_job_definition(name)
                except FileNotFoundError:
                    pass
                return None
            raise ProcessFailure(result.returncode, result.args, result.stdout, result.stderr)
        lines = result.stdout.strip().splitlines()
        mismatch = ValueError('Loaded W1 service has an unexpected launchd definition: ' + target)
        if result.stderr.strip() or len(lines) < 3 or lines[0] != f'{target} = {{' or lines[-1] != '}':
            raise mismatch
        indent = re.match(r'[ \t]+', lines[1])
        if indent is None:
            raise mismatch
        indent = indent.group()
        fields = [line[len(indent):] for line in lines[1:-1]
                  if line.startswith(indent) and line[len(indent):len(indent) + 1] not in (' ', '\t')]
        for field, expected in [('path', self.state / (name + '.plist')),
                                ('program', self.argv[name][0]), ('working directory', self.state)]:
            key = field + ' = '
            if [line for line in fields if line.startswith(key)] != [key + str(expected)]:
                raise mismatch
        if fields.count('arguments = {') != 1:
            raise mismatch
        start = lines.index(indent + 'arguments = {') + 1
        try:
            end = lines.index(indent + '}', start)
        except ValueError:
            raise mismatch from None
        if lines[start:end] != [indent * 2 + arg for arg in self.argv[name]]:
            raise mismatch
        try:
            self.validate_job_definition(name)
        except FileNotFoundError:
            raise mismatch from None
        pids = [line[6:] for line in fields if line.startswith('pid = ')]
        if len(pids) > 1 or (pids and not re.fullmatch(r'[0-9]+', pids[0])):
            raise ValueError('Unexpected launchd PID output')
        return int(pids[0]) if pids else 0

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

    def close(self, *, remove_before=()):
        self.validate_directories(stopping=True)
        loaded = {name: self.loaded_pid(name, allow_missing=True) for name in self.attempted}
        for name in reversed(self.attempted):
            if loaded[name] is not None:
                try:
                    run('launchctl', 'bootout', self.targets[name])
                except (ProcessFailure, subprocess.TimeoutExpired):
                    if self.loaded_pid(name, allow_missing=True) is not None:
                        raise
        deadline = time.monotonic() + 15
        while True:
            remaining = [name for name in self.attempted
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
                sync_directory(path.parent)
        for path in remove_before:
            path.unlink()
        sync_directory(self.manifest.parent)
        self.manifest.unlink()
        sync_directory(self.manifest.parent)


def w1_slot_paths(manifest):
    recovery = w1_recovery_path(manifest)
    return manifest, recovery, staged_record(manifest), staged_record(recovery)


def close_w1(manifest, *, fixture=False, lab_id=None):
    if manifest.parent.exists():
        with w1_lock(manifest):
            close_w1_locked(manifest, fixture=fixture, lab_id=lab_id)


def close_w1_locked(manifest, *, fixture, lab_id=None):
    ready_path, recovery_path, *pending = w1_slot_paths(manifest)
    pending = [path for path in pending if os.path.lexists(path)]
    for path in pending:
        private_path(path, stat.S_ISREG, 0o600)
    ready = (W1Record(ready_path, fixture=fixture, stopping=True)
             if os.path.lexists(ready_path) else None)
    recovery = (W1Record(recovery_path, fixture=fixture, stopping=True, recovery=True)
                if os.path.lexists(recovery_path) else None)
    record = recovery or ready
    if record is None:
        for path in pending:
            path.unlink()
        return
    if lab_id is not None and record.record['id'] != lab_id:
        raise ValueError('W1 recovery belongs to a different lab')
    if ready and recovery:
        fields = ('id', 'state_identity', 'catalog_identity', 'startup_argv')
        if (not recovery.record['startup_complete']
                or any(ready.record[field] != recovery.record[field] for field in fields)):
            raise ValueError('W1 ready manifest and recovery record disagree')
        pending.append(ready_path)
    record.close(remove_before=pending)


class Lab:
    def __init__(self, *, w1=False, fixture=False, manifest=None, pressure=False, writable_smoke=False):
        if sys.platform != 'darwin':
            raise RuntimeError('launchd lifecycle proof requires macOS')
        if fixture and not w1:
            raise ValueError('Recovery fixtures require W1 jobs')
        if pressure and (not w1 or fixture or manifest is not None):
            raise ValueError('Pressure requires its own W1 lab')
        self.host = ROOT / 'target/debug/host-agent'
        self.tmux = shutil.which('tmux')
        tmux_remedy = f'Put tmux {W1_TMUX_VERSION} first on PATH and retry setup.'
        if (w1 or writable_smoke) and not self.tmux:
            raise RuntimeError(f'W1 requires tmux {W1_TMUX_VERSION} on PATH. Observed unavailable. {tmux_remedy}')
        if not self.host.is_file() or not self.tmux:
            raise RuntimeError('build host-agent and install tmux first')
        self.w1 = w1
        self.writable_smoke = writable_smoke
        self.fixture = fixture
        self.id = uuid.uuid4().hex[:12]
        self.catalog = None
        self.catalog_created = False
        self.domain = f'gui/{os.getuid()}'
        self.prefix = (W1_LABEL if w1 else 'dev.remotesessions.p0.') + self.id
        self.loaded = []
        if w1:
            self.manifest = w1_pressure_manifest(self.id) if pressure else (manifest or W1_MANIFEST)
            self.recovery_path = w1_recovery_path(self.manifest)
            self.recovery = None
            self.cleanup_confirmed = False
            self.state, self.catalog = w1_paths(self.id)
            if (self.catalog.parent.resolve(strict=True) != self.catalog.parent
                    or self.catalog.parent.stat().st_dev != internal_device()):
                raise RuntimeError('W1 requires a real Application Support directory on the internal data volume')
        if w1 or writable_smoke:
            try:
                self.tmux = str(Path(self.tmux).resolve(strict=True))
            except (OSError, RuntimeError) as error:
                raise RuntimeError(f'W1 requires tmux {W1_TMUX_VERSION} at {self.tmux}. Observed unavailable. '
                                   f'{tmux_remedy}') from error
            self.require_writable_tmux_binary()
        if w1:
            self.state.mkdir(mode=0o700)
            self.state_identity = directory_identity(self.state)
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

    def require_writable_tmux_binary(self):
        require_w1_tmux_version(
            lambda: run(self.tmux, '-V', check=False), f'tmux {W1_TMUX_VERSION}',
            f'the selected binary at {self.tmux}',
            f'Put tmux {W1_TMUX_VERSION} first on PATH and retry setup.',
        )

    def require_writable_tmux_server(self, *, wait_until_ready=False):
        def server_version():
            if wait_until_ready:
                self.wait_for_tmux()
            return self.tmux_call('display-message', '-p', '#{version}')

        remedy = (f'Run the scoped down command for this lab, then retry with tmux {W1_TMUX_VERSION}.'
                  if self.w1 else f'Put tmux {W1_TMUX_VERSION} first on PATH and rerun make check-desktop.')
        require_w1_tmux_version(
            server_version, W1_TMUX_VERSION,
            f'the dedicated tmux server at {self.state / "tmux.sock"} using {self.tmux}', remedy,
        )

    def w1_record(self):
        return {
            **{key: value for key, value in self.recovery.items()
               if key not in ('attempted', 'startup_complete')},
            'catalog_provision_argv': [str(self.service_host), '--catalog-dir', str(self.catalog), 'catalog-init'],
            'catalog_startup': catalog_message(self.state),
        }

    def prepare_recovery(self):
        self.catalog.mkdir(mode=0o700)
        self.catalog_identity = directory_identity(self.catalog)
        self.catalog_created = True
        if (self.catalog_identity[0] != internal_device()
                or directory_identity(self.state) != self.state_identity):
            raise ValueError('W1 lab directory identity changed')
        agent_tmux = str(self.state / 'tmux-fixture') if self.fixture else self.tmux
        self.startup_argv = job_argv(self.state, self.catalog, self.tmux, agent_tmux)
        self.recovery = {
            'id': self.id, 'fixture': self.fixture, 'state_dir': str(self.state),
            'catalog_dir': str(self.catalog), 'tmux': self.tmux,
            'targets': [self.domain + '/' + self.prefix + '.' + name for name in W1_JOBS],
            'startup_argv': self.startup_argv, 'state_identity': self.state_identity,
            'catalog_identity': self.catalog_identity, 'attempted': {}, 'startup_complete': False,
        }
        write_w1_manifest(self.recovery_path, self.recovery)
        run(self.service_host, '--catalog-dir', self.catalog, 'catalog-init')

    def publish_w1_manifest(self, *, runtimes=None):
        with w1_lock(self.manifest):
            recovery = W1Record(self.recovery_path, fixture=self.fixture, recovery=True)
            if recovery.record != self.recovery or not recovery.record['startup_complete']:
                raise ValueError('W1 startup has not completed for this lab')
            record = self.w1_record()
            if runtimes is not None:
                record['runtimes'] = runtimes
            write_w1_manifest(self.manifest, record)
            return record

    def start(self):
        if self.w1:
            try:
                with w1_lock(self.manifest) as lock:
                    return self.start_jobs(launch_fds=(lock,))
            except (Exception, KeyboardInterrupt):
                if not self.recovery or not os.path.lexists(self.recovery_path):
                    self.close_unstarted()
                else:
                    print(f'W1 startup interrupted. Recovery retained at {self.recovery_path}.', file=sys.stderr)
                raise
        return self.start_jobs()

    def start_jobs(self, *, launch_fds=()):
        try:
            if self.w1:
                if any(os.path.lexists(path) for path in w1_slot_paths(self.manifest)):
                    raise RuntimeError(f'W1 lab already recorded at {self.manifest}; run its down command first')
                self.prepare_recovery()
            for name, args in self.startup_argv.items():
                label = self.prefix + '.' + name
                path = self.state / (name + '.plist')
                path_env = self.agent_path if name == 'agent' else os.environ['PATH']
                definition = job_definition(self.state, self.prefix, name, args, path_env)
                if self.w1:
                    with private_output(path, 'wb') as output:
                        plistlib.dump(definition, output)
                        output.flush()
                        os.fsync(output.fileno())
                    sync_directory(self.state)
                    self.recovery['attempted'][name] = definition
                    write_w1_manifest(self.recovery_path, self.recovery, replace=True)
                else:
                    path.write_bytes(plistlib.dumps(definition))
                    path.chmod(0o600)
                run('launchctl', 'bootstrap', self.domain, path, pass_fds=launch_fds)
                self.loaded.append(self.domain + '/' + label)
            if self.w1 or self.writable_smoke:
                self.require_writable_tmux_server(wait_until_ready=True)
            wait_for(lambda: self.call('health').get('agent_pid'))
            if self.w1:
                if not wait_for(lambda: catalog_message(self.state)).startswith('local catalog ready ('):
                    raise RuntimeError('W1 catalog failed to open')
                self.recovery['startup_complete'] = True
                write_w1_manifest(self.recovery_path, self.recovery, replace=True)
            elif not self.writable_smoke:
                self.wait_for_tmux()
            return self
        except (Exception, KeyboardInterrupt):
            if self.w1:
                raise
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

    def wait_for_tmux(self):
        wait_for(lambda: self.tmux_call('show-options', '-s', 'exit-empty').returncode == 0)

    def close(self):
        if self.w1:
            if self.recovery is None:
                self.close_unstarted()
            elif not self.cleanup_confirmed:
                with w1_lock(self.manifest):
                    close_w1_locked(self.manifest, fixture=self.fixture, lab_id=self.id)
                    if any(os.path.lexists(path) for path in (self.state, self.catalog)):
                        raise RuntimeError('W1 lab paths remain after cleanup')
                    self.loaded.clear()
                    self.cleanup_confirmed = True
            return
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

    def close_unstarted(self):
        owned = [(self.state, self.state_identity)]
        if self.catalog_created:
            owned.append((self.catalog, self.catalog_identity))
        for path, identity in owned:
            if os.path.lexists(path):
                if directory_identity(path) != identity:
                    raise ValueError(f'W1 directory identity changed: {path}')
                shutil.rmtree(path)

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
    parser.add_argument('--pressure', metavar='LAB_ID', help='Recover a disposable pressure lab')
    args = parser.parse_args()
    if args.pressure is not None and (
            not args.w1 or args.action != 'down' or not W1_ID.fullmatch(args.pressure)):
        parser.error('--pressure requires down --w1 and a valid lab ID')
    if args.w1:
        if args.action == 'down':
            manifest = w1_pressure_manifest(args.pressure) if args.pressure else W1_MANIFEST
            close_w1(manifest, lab_id=args.pressure)
            print('W1 runtime lab stopped.')
        else:
            lab = Lab(w1=True, manifest=W1_MANIFEST).start()
            try:
                record = lab.publish_w1_manifest()
            except (Exception, KeyboardInterrupt):
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
