"""Stdlib checks for lab scoping and the native recovery evidence contract."""
import argparse
from contextlib import closing, contextmanager, nullcontext, redirect_stderr, redirect_stdout
from copy import deepcopy
import io
import json
import os
from pathlib import Path
import plistlib
import shutil
import socket
import sqlite3
import stat
import subprocess
import sys
import tempfile
import threading
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch
from uuid import UUID, uuid4

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'scripts'))

import desktop_lab
import runtime_lab
import w1_recovery
import w1_terminal_pressure
from w1_recovery import (CASE_CHECKS, ResponseLoss, Verdict, read_frame,
                         case_status, catalog_snapshot, pane, prepare_kingston_workspace,
                         request_frame, response_state)


class ManifestTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        root = Path(directory.name).resolve()
        self.manifest = root / 'record.json'
        self.state, self.catalog = root / 'state', root / 'catalog'
        for path in self.state, self.catalog:
            path.mkdir(mode=0o700)
        self.tmux = '/usr/local/bin/tmux'
        self.lab_id = '012345abcdef'
        self.prefix = runtime_lab.W1_LABEL + self.lab_id
        self.targets = [f'gui/{os.getuid()}/{self.prefix}.{name}' for name in ('tmux', 'agent')]
        self.argv = runtime_lab.job_argv(self.state, self.catalog, self.tmux, self.tmux)
        self.record = {
            'id': self.lab_id, 'fixture': False, 'state_dir': str(self.state),
            'catalog_dir': str(self.catalog), 'tmux': self.tmux,
            'targets': self.targets, 'startup_argv': self.argv,
            'state_identity': runtime_lab.directory_identity(self.state),
            'catalog_identity': runtime_lab.directory_identity(self.catalog),
        }
        for name, argv in self.argv.items():
            plist = self.state / (name + '.plist')
            plist.write_bytes(plistlib.dumps({
                'Label': self.prefix + '.' + name,
                'ProgramArguments': argv, 'WorkingDirectory': str(self.state),
            }))
            plist.chmod(0o600)

    def save(self):
        runtime_lab.write_w1_manifest(self.manifest, self.record)

    def load(self, **options):
        with (patch.object(runtime_lab, 'w1_paths', return_value=(self.state, self.catalog)),
              patch.object(runtime_lab, 'internal_device', return_value=self.catalog.stat().st_dev)):
            return runtime_lab.W1Record(self.manifest, **options)

    def printed(self, name, *, pid=21, path=None, argv=None):
        target = self.targets[('tmux', 'agent').index(name)]
        args = self.argv[name] if argv is None else argv
        path = self.state / (name + '.plist') if path is None else path
        fields = [f'{target} = {{', '\tactive count = 1', f'\tpath = {path}',
                  f'\tprogram = {args[0]}', '\targuments = {',
                  *('\t\t' + arg for arg in args), '\t}',
                  f'\tworking directory = {self.state}', f'\tpid = {pid}', '}']
        return '\n'.join(fields) + '\n'

    def absent(self, command, action, target, *, check):
        self.assertEqual((command, action, check), ('launchctl', 'print', False))
        self.assertIn(target, self.targets)
        label = target.rsplit('/', 1)[1]
        return subprocess.CompletedProcess((command, action, target), 113, '',
            f'Bad request.\nCould not find service "{label}" in domain for user gui: {os.getuid()}\n')

    def prepare_start(self):
        shutil.rmtree(self.state)
        self.catalog.rmdir()
        root = self.manifest.parent
        host = root / 'target/debug/host-agent'
        host.parent.mkdir(parents=True)
        host.touch()
        (root / 'config').mkdir()
        (root / 'config/tmux.conf').touch()
        for replacement in [
            patch.object(runtime_lab, 'ROOT', root),
            patch.object(runtime_lab, 'W1_MANIFEST', self.manifest),
            patch.object(runtime_lab, 'w1_paths', side_effect=self.resolve_paths),
            patch.object(runtime_lab, 'internal_device', return_value=root.stat().st_dev),
            patch.object(runtime_lab.shutil, 'which', return_value='/dev/null'),
            patch.object(runtime_lab.uuid, 'uuid4', return_value=UUID(self.lab_id + '0' * 20)),
            patch.object(runtime_lab.sys, 'platform', 'darwin'),
        ]:
            replacement.start()
            self.addCleanup(replacement.stop)
        self.tmux = '/dev/null'
        self.argv = runtime_lab.job_argv(self.state, self.catalog, self.tmux, self.tmux)

    def resolve_paths(self, lab_id):
        self.assertEqual(lab_id, self.lab_id)
        return self.state, self.catalog

    def desktop_down(self, *, fixture=False):
        argv = ['desktop_lab.py', 'down', '--w1'] + (['--fixture'] if fixture else [])
        with (patch.object(desktop_lab, 'FIXTURE_MANIFEST' if fixture else 'W1_MANIFEST', self.manifest),
              patch.object(runtime_lab, 'w1_paths', side_effect=self.resolve_paths),
              patch.object(runtime_lab, 'internal_device', return_value=self.manifest.parent.stat().st_dev),
              patch.object(sys, 'argv', argv)):
            desktop_lab.main()

    def test_manifest_is_private_exclusive_and_validates_configuration(self):
        self.save()
        self.assertEqual(stat.S_IMODE(self.manifest.stat().st_mode), 0o600)
        with self.assertRaises(FileExistsError):
            self.save()
        self.assertEqual(self.load().argv['agent'][-3:], ['--catalog-dir', str(self.catalog), 'serve'])
        self.assertEqual(self.load().targets['agent'], self.targets[1])
        with self.assertRaisesRegex(ValueError, 'Unexpected W1 lab mode'):
            self.load(fixture=True)

    def test_refuses_foreign_path_or_domain_before_launchd(self):
        for field, value in [
            ('state_dir', str(self.state.parent)),
            ('catalog_dir', str(self.catalog.parent)),
            ('targets', [self.targets[0], self.targets[1] + '.foreign']),
            ('targets', [self.targets[0], 'system/' + self.prefix + '.agent']),
            ('startup_argv', {'agent': ['other-agent', 'serve']}),
        ]:
            with self.subTest(field=field, value=value):
                original = self.record[field]
                self.record[field] = value
                self.save()
                with patch.object(runtime_lab, 'run') as launchctl:
                    with self.assertRaises(ValueError):
                        self.load()
                    launchctl.assert_not_called()
                self.manifest.unlink()
                self.record[field] = original

    def test_refuses_changed_directory_and_manifest_symlink(self):
        self.save()
        original = self.catalog.with_suffix('.original')
        self.catalog.rename(original)
        self.catalog.mkdir(mode=0o700)
        with self.assertRaisesRegex(ValueError, 'identity changed'):
            self.load()
        moved = self.manifest.with_suffix('.original')
        self.manifest.rename(moved)
        self.manifest.symlink_to(moved)
        with self.assertRaisesRegex(ValueError, 'Unexpected private lab path'):
            self.load()

    def test_agent_restart_targets_only_validated_job(self):
        self.save()
        lab = self.load()
        target = lab.targets['agent']

        def fake_run(*args, **_):
            if args[:2] == ('launchctl', 'print'):
                return subprocess.CompletedProcess(args, 0, self.printed('agent', pid=next(pids)), '')
            return subprocess.CompletedProcess(args, 0, '', '')

        pids = iter([21, 22])
        with (patch.object(runtime_lab, 'run', side_effect=fake_run) as launchctl,
              patch.object(lab, 'call', return_value={'agent_pid': 22})):
            self.assertEqual(lab.restart_agent(), [21, 22])
        self.assertEqual([c.args for c in launchctl.call_args_list], [
            ('launchctl', 'print', target), ('launchctl', 'kickstart', '-k', target),
            ('launchctl', 'print', target),
        ])

    def test_loaded_foreign_definition_is_never_unloaded(self):
        self.save()
        lab = self.load()
        with patch.object(runtime_lab, 'run', return_value=subprocess.CompletedProcess(
                [], 0, 'path = /tmp/foreign.plist\npid = 21\n', '')) as launchctl:
            with self.assertRaisesRegex(ValueError, 'unexpected launchd definition'):
                lab.close()
        self.assertEqual([c.args[:2] for c in launchctl.call_args_list], [('launchctl', 'print')])
        self.assertTrue(self.manifest.is_file())

    def test_restart_refuses_changed_agent_definition(self):
        self.save()
        lab = self.load()
        plist = self.state / 'agent.plist'
        definition = plistlib.loads(plist.read_bytes())
        definition['ProgramArguments'][-2] = str(self.catalog.parent)
        plist.write_bytes(plistlib.dumps(definition))
        with patch.object(runtime_lab, 'run', return_value=subprocess.CompletedProcess(
                [], 0, self.printed('agent'), '')) as launchctl:
            with self.assertRaisesRegex(ValueError, 'Unexpected W1 service definition'):
                lab.restart_agent()
        launchctl.assert_called_once_with('launchctl', 'print', lab.targets['agent'], check=False)

    def test_teardown_waits_for_both_jobs_and_removes_only_their_private_paths(self):
        self.save()
        lab = self.load(stopping=True)
        loaded = set(self.targets)
        pending_prints = dict(zip(self.targets, (2, 1)))

        def fake_run(*args, **kwargs):
            target = args[2]
            if args[1] == 'bootout':
                loaded.remove(target)
                return subprocess.CompletedProcess(args, 0, '', '')
            if target not in loaded:
                if pending_prints[target] == 0:
                    return self.absent(*args, **kwargs)
                self.assertTrue(all(path.exists() for path in (self.manifest, self.state, self.catalog)))
                pending_prints[target] -= 1
            return subprocess.CompletedProcess(args, 0, self.printed(target.rsplit('.', 1)[1]), '')

        with (patch.object(runtime_lab, 'run', side_effect=fake_run) as launchctl,
              patch.object(runtime_lab.time, 'sleep') as sleep):
            lab.close()
        self.assertEqual(sleep.call_count, 2)
        bootouts = [c.args for c in launchctl.call_args_list if c.args[1] == 'bootout']
        self.assertEqual(bootouts, [('launchctl', 'bootout', target) for target in reversed(self.targets)])
        self.assertFalse(self.manifest.exists() or self.state.exists() or self.catalog.exists())
        self.assertTrue(self.manifest.parent.is_dir())

    def test_teardown_timeout_preserves_manifest_and_directories(self):
        self.save()
        lab = self.load(stopping=True)
        with (patch.object(lab, 'loaded_pid', return_value=0) as loaded_pid,
              patch.object(runtime_lab, 'run'),
              patch.object(runtime_lab.time, 'monotonic', side_effect=[10, 25]),
              patch.object(runtime_lab.time, 'sleep') as sleep):
            with self.assertRaisesRegex(TimeoutError, 'W1 jobs are still loaded'):
                lab.close()
        self.assertEqual(loaded_pid.call_count, 4)
        sleep.assert_not_called()
        self.assertTrue(all(path.exists() for path in (self.manifest, self.state, self.catalog)))

    def test_teardown_propagates_unexpected_print_error_and_preserves_paths(self):
        self.save()
        lab = self.load(stopping=True)
        prints = iter([
            (0, self.printed('tmux'), ''),
            (0, self.printed('agent', pid=22), ''),
            (0, self.printed('tmux'), ''),
            (1, '', 'Operation not permitted'),
        ])

        def fake_run(*args, **_):
            result = next(prints) if args[1] == 'print' else (0, '', '')
            return subprocess.CompletedProcess(args, *result)

        with (patch.object(runtime_lab, 'run', side_effect=fake_run),
              patch.object(runtime_lab.time, 'sleep') as sleep):
            with self.assertRaisesRegex(runtime_lab.ProcessFailure, 'Operation not permitted'):
                lab.close()
        sleep.assert_not_called()
        self.assertTrue(all(path.exists() for path in (self.manifest, self.state, self.catalog)))

    def test_bootstrap_registered_before_timeout(self):
        self.prepare_start()
        recovery = runtime_lab.w1_recovery_path(self.manifest)
        loaded = set()

        def fake_run(*args, **kwargs):
            if args[:2] == ('launchctl', 'bootstrap'):
                self.assertEqual(args[2:], (f'gui/{os.getuid()}', self.state / 'tmux.plist'))
                self.assertEqual(list(json.loads(recovery.read_text())['attempted']), ['tmux'])
                loaded.add(self.targets[0])
                raise subprocess.TimeoutExpired(args, 15)
            if args[:2] == ('launchctl', 'print'):
                if args[2] in loaded:
                    return subprocess.CompletedProcess(args, 0, self.printed('tmux'), '')
                return self.absent(*args, **kwargs)
            if args[:2] == ('launchctl', 'bootout'):
                loaded.remove(args[2])
            return subprocess.CompletedProcess(args, 0, '', '')

        with (patch.object(desktop_lab, 'W1_MANIFEST', self.manifest),
              patch.object(runtime_lab, 'run', side_effect=fake_run) as command):
            with self.assertRaises(subprocess.TimeoutExpired):
                desktop_lab.start_w1()
            self.assertEqual(loaded, {self.targets[0]})
            self.assertTrue(recovery.is_file())
            self.assertFalse(self.manifest.exists())
            command.reset_mock()
            self.desktop_down()
        self.assertEqual([call.args for call in command.call_args_list], [
            ('launchctl', 'print', self.targets[0]), ('launchctl', 'bootout', self.targets[0]),
            ('launchctl', 'print', self.targets[0]),
        ])
        self.assertEqual(loaded, set())
        self.assertFalse(any(path.exists() for path in (recovery, self.state, self.catalog)))

    def test_second_job_bootstrap_interrupted(self):
        self.prepare_start()
        lab = runtime_lab.Lab(w1=True, manifest=self.manifest)
        recovery = runtime_lab.w1_recovery_path(self.manifest)
        loaded = set()

        def fake_run(*args, **kwargs):
            if args[:2] == ('launchctl', 'bootstrap'):
                name = args[3].stem
                record = json.loads(recovery.read_text())
                self.assertEqual(list(record['attempted']), ['tmux', 'agent'] if name == 'agent' else ['tmux'])
                self.assertFalse(record['startup_complete'])
                self.assertEqual(record['attempted'][name], plistlib.loads(args[3].read_bytes()))
                if name == 'agent':
                    raise KeyboardInterrupt()
                loaded.add(self.targets[0])
            elif args[:2] == ('launchctl', 'print'):
                if args[2] in loaded:
                    return subprocess.CompletedProcess(args, 0, self.printed('tmux'), '')
                return self.absent(*args, **kwargs)
            elif args[:2] == ('launchctl', 'bootout'):
                loaded.remove(args[2])
            return subprocess.CompletedProcess(args, 0, '', '')

        with patch.object(runtime_lab, 'run', side_effect=fake_run) as command:
            with self.assertRaises(KeyboardInterrupt):
                lab.start()
            self.assertTrue(recovery.is_file())
            self.assertFalse(self.manifest.exists())
            self.assertEqual(loaded, {self.targets[0]})
            command.reset_mock()
            self.desktop_down()
        self.assertEqual([call.args[2] for call in command.call_args_list if call.args[1] == 'bootout'],
                         [self.targets[0]])
        self.assertEqual(loaded, set())
        self.assertFalse(recovery.exists())

    def test_private_recovery_available_before_manifest(self):
        self.prepare_start()
        recovery = runtime_lab.w1_recovery_path(self.manifest)
        with (patch.object(runtime_lab, 'run'),
              patch.object(runtime_lab, 'wait_for', side_effect=[123, True, 'local catalog ready (fixture)'])):
            runtime_lab.Lab(w1=True, manifest=self.manifest).start()
        self.assertFalse(self.manifest.exists())
        self.assertEqual(stat.S_IMODE(recovery.stat().st_mode), 0o600)
        self.assertEqual(recovery.stat().st_nlink, 1)
        self.assertTrue(json.loads(recovery.read_text())['startup_complete'])
        with self.assertRaises(FileNotFoundError):
            runtime_lab.W1Record(self.manifest)
        with patch.object(runtime_lab, 'run', side_effect=self.absent) as command:
            self.desktop_down()
        self.assertEqual([call.args for call in command.call_args_list], [
            ('launchctl', 'print', target) for target in self.targets * 2
        ])
        self.assertFalse(any(path.exists() for path in (recovery, self.manifest, self.state, self.catalog)))

    def test_down_after_partial_plist_cleanup(self):
        self.prepare_start()
        with (patch.object(runtime_lab, 'run'),
              patch.object(runtime_lab, 'catalog_message', return_value='local catalog ready (fixture)'),
              patch.object(runtime_lab, 'wait_for', side_effect=[123, True, 'local catalog ready (fixture)'])):
            runtime_lab.Lab(w1=True, manifest=self.manifest).start().publish_w1_manifest()
        recovery = runtime_lab.w1_recovery_path(self.manifest)

        def partial_cleanup(path):
            self.assertEqual(path, self.state)
            (path / 'tmux.plist').unlink()
            raise KeyboardInterrupt()

        with patch.object(runtime_lab, 'run', side_effect=self.absent) as command:
            with (patch.object(runtime_lab.shutil, 'rmtree', side_effect=partial_cleanup),
                  self.assertRaises(KeyboardInterrupt)):
                self.desktop_down()
            self.assertTrue(all(path.exists() for path in (recovery, self.manifest, self.state, self.catalog)))
            self.assertFalse((self.state / 'tmux.plist').exists())
            self.desktop_down()
            self.assertFalse(any(path.exists() for path in (recovery, self.manifest, self.state, self.catalog)))
            command.reset_mock()
            self.desktop_down()
            command.assert_not_called()

    def test_down_retries_an_interrupted_bootout(self):
        self.save()
        loaded = set(self.targets)
        bootouts = []

        def fake_run(*args, **kwargs):
            target = args[2]
            if args[1] == 'bootout':
                bootouts.append(target)
                loaded.remove(target)
                if target == self.targets[1]:
                    raise KeyboardInterrupt()
                raise subprocess.TimeoutExpired(args, 15)
            if target in loaded:
                return subprocess.CompletedProcess(args, 0, self.printed(target.rsplit('.', 1)[1]), '')
            return self.absent(*args, **kwargs)

        with patch.object(runtime_lab, 'run', side_effect=fake_run):
            with self.assertRaises(KeyboardInterrupt):
                self.desktop_down()
            self.assertTrue(all(path.exists() for path in (self.manifest, self.state, self.catalog)))
            self.assertEqual(loaded, {self.targets[0]})
            self.desktop_down()
        self.assertEqual(bootouts, self.targets[::-1])
        self.assertFalse(any(path.exists() for path in (self.manifest, self.state, self.catalog)))

    def test_down_holds_on_foreign_or_ambiguous_job(self):
        self.save()
        plist = self.state / 'agent.plist'
        before = plist.read_bytes()
        unrelated = self.manifest.parent / 'unrelated'
        unrelated.touch()
        for stdout, code, stderr, missing, error in [
            (self.printed('agent', path='/tmp/foreign.plist'), 0, '', True, ValueError),
            (self.printed('agent', argv=['/tmp/foreign-agent', 'serve']), 0, '', False, ValueError),
            (self.printed('agent'), 0, '', True, ValueError),
            ('', 1, 'Operation not permitted', True, runtime_lab.ProcessFailure),
            ('', 113, 'Could not find domain for user gui', True, runtime_lab.ProcessFailure),
            ('', 113, 'Could not find service "different" in domain for user gui',
             True, runtime_lab.ProcessFailure),
        ]:
            with self.subTest(stdout=stdout, stderr=stderr):
                if missing:
                    plist.unlink()

                def fake_run(*args, **_):
                    if args[2] == self.targets[0]:
                        return subprocess.CompletedProcess(args, 0, self.printed('tmux'), '')
                    return subprocess.CompletedProcess(args, code, stdout, stderr)

                with patch.object(runtime_lab, 'run', side_effect=fake_run) as command:
                    with self.assertRaises(error):
                        self.desktop_down()
                self.assertEqual([call.args for call in command.call_args_list], [
                    ('launchctl', 'print', self.targets[0]), ('launchctl', 'print', self.targets[1]),
                ])
                self.assertEqual(plist.exists(), not missing)
                self.assertTrue(all(path.exists() for path in (self.manifest, self.state, self.catalog, unrelated)))
                if missing:
                    plist.write_bytes(before)
                    plist.chmod(0o600)

    def test_start_provisions_before_bootstrap_and_uses_exact_path(self):
        self.prepare_start()
        lab = runtime_lab.Lab(w1=True, manifest=self.manifest)
        with (patch.object(runtime_lab, 'run') as command,
              patch.object(runtime_lab, 'wait_for', side_effect=[123, True, 'local catalog ready (fixture)'])):
            lab.start()
        self.assertEqual(command.call_args_list[0].args,
                         (lab.service_host, '--catalog-dir', self.catalog, 'catalog-init'))
        self.assertEqual(command.call_args_list[1].args[:2], ('launchctl', 'bootstrap'))
        self.assertEqual(plistlib.loads((self.state / 'agent.plist').read_bytes())['ProgramArguments'],
                         self.argv['agent'])
        self.assertEqual(lab.loaded, self.targets)
        with patch.object(runtime_lab, 'catalog_message', return_value='local catalog ready (fixture)'):
            lab.publish_w1_manifest()
        self.assertEqual(list(runtime_lab.W1Record(self.manifest).targets.values()), self.targets)

    def test_fixture_and_pressure_callers_preserve_recoverable_startup(self):
        for caller in ('fixture', 'pressure'):
            with self.subTest(caller=caller):
                if self.state.exists():
                    self.prepare_start()
                probe = self.manifest.parent / 'target/debug/examples/probe'
                probe.parent.mkdir(exist_ok=True)
                probe.touch()
                pressure = caller == 'pressure'
                manifest = runtime_lab.w1_pressure_manifest(self.lab_id) if pressure else self.manifest
                recovery = runtime_lab.w1_recovery_path(manifest)
                report = {}
                loaded = set()

                def fake_run(*args, **kwargs):
                    if args[:2] == ('launchctl', 'bootstrap'):
                        loaded.add(self.targets[0])
                        raise subprocess.TimeoutExpired(args, 15)
                    if args[:2] == ('launchctl', 'print'):
                        if args[2] in loaded:
                            return subprocess.CompletedProcess(args, 0, self.printed('tmux'), '')
                        return self.absent(*args, **kwargs)
                    if args[:2] == ('launchctl', 'bootout'):
                        loaded.remove(args[2])
                    return subprocess.CompletedProcess(args, 0, '', '')

                with (patch.object(runtime_lab, 'run', side_effect=fake_run),
                      patch.object(w1_recovery, 'FIXTURE_MANIFEST', manifest),
                      patch.object(w1_terminal_pressure, 'ROOT', self.manifest.parent),
                      patch.object(w1_terminal_pressure, 'lab_host_artifact', return_value={}),
                      patch.object(w1_terminal_pressure, 'sample_clock', return_value={})):
                    with self.assertRaises(subprocess.TimeoutExpired):
                        if pressure:
                            w1_terminal_pressure.prove(SimpleNamespace(), report,
                                                       {'artifacts': {'probe': {'path': str(probe)}}})
                        else:
                            with w1_recovery.fixture_jobs():
                                self.fail('Fixture startup unexpectedly succeeded')
                    self.assertFalse(manifest.exists())
                    self.assertEqual(json.loads(recovery.read_text())['fixture'], not pressure)
                    if pressure:
                        self.assertEqual(report['lab_id'], self.lab_id)
                        self.assertNotIn('cleanup', report)
                        argv = ['runtime_lab.py', 'down', '--w1', '--pressure', self.lab_id]
                        with patch.object(sys, 'argv', argv):
                            runtime_lab.main()
                    else:
                        self.desktop_down(fixture=True)
                self.assertEqual(loaded, set())
                self.assertFalse(any(path.exists() for path in (recovery, self.state, self.catalog)))

    def test_p0_run_uses_historical_manifest(self):
        self.manifest.write_text(json.dumps({'state_dir': '/tmp/p0', 'tmux': self.tmux}))
        with (patch.object(desktop_lab, 'MANIFEST', self.manifest),
              patch.object(desktop_lab, 'run_app') as run_app,
              patch.object(sys, 'argv', ['desktop_lab.py', 'run', '--with-clis'])):
            desktop_lab.main()
        run_app.assert_called_once_with('/tmp/p0', self.tmux)

    def test_w1_rejects_p0_provider_options(self):
        with (patch.object(sys, 'argv', ['desktop_lab.py', 'up', '--w1', '--with-clis']),
              patch.object(argparse.ArgumentParser, 'error', side_effect=ValueError) as error):
            with self.assertRaises(ValueError):
                desktop_lab.main()
        error.assert_called_once_with('--with-clis is for the P0 lab')


class LossTests(unittest.TestCase):
    @staticmethod
    def runtime(workspace_id=None, *, runtime_id=None):
        session_id = uuid4()
        runtime_id = uuid4() if runtime_id is None else runtime_id
        return {'operation_id': str(uuid4()) if workspace_id is not None else None, 'runtime_id': str(runtime_id),
                'workspace_id': workspace_id, 'session_id': str(session_id),
                'origin': 'managed' if workspace_id else 'legacy', 'pane_id': '%2', 'pane_pid': 42,
                'exited': False, 'command': 'fixture', 'width': 80, 'height': 24,
                'tmux_name': f'rs_{session_id.hex}_{runtime_id.hex}'}

    def test_drop_only_one_complete_confirmed_create_and_forward_status_inventory(self):
        key = str(uuid4())
        workspace_id = str(uuid4())
        create = {'product': {'op': 'create_managed', 'key': key, 'workspace_id': workspace_id,
                              'profile': 'codex'}}
        runtime = self.runtime(workspace_id)
        reply = request_frame({'product': {'result': 'create', 'state': {
            'state': 'confirmed', 'current': 'live', 'outcome': {'runtime': runtime}}}})
        failure = request_frame({'product': {'result': 'create', 'state': {
            'state': 'failed', 'issue': 'workspace_changed'}}})
        status = {'product': {'op': 'create_status', 'key': key}}
        inventory = request_frame({'result': 'runtimes', 'runtimes': [runtime]})
        loss = ResponseLoss()
        self.assertEqual(loss.response(create, reply)[0], reply)
        loss.arm('codex')
        self.assertEqual(loss.response(create, failure)[0], failure)
        wrong_target = json.loads(reply)
        wrong_target['product']['state']['outcome']['runtime']['workspace_id'] = str(uuid4())
        wrong_target = request_frame(wrong_target)
        self.assertEqual(loss.response(create, wrong_target)[0], wrong_target)
        self.assertEqual(loss.response(status, reply)[0], reply)
        with self.assertRaises(ValueError):
            loss.response(create, reply[:-1])
        self.assertIsNone(loss.response(create, reply)[0])
        self.assertEqual(loss.response(status, reply)[0], reply)
        self.assertEqual(loss.response({'op': 'list'}, inventory)[0], inventory)
        self.assertEqual(loss.response(create, reply)[0], reply)
        self.assertEqual(loss.drops, [{'key': key, 'runtime': runtime}])
        self.assertEqual(loss.forwarded('list'), 0)
        self.assertEqual(response_state(json.loads(failure))['state'], 'failed')

    def test_bounded_newline_frames(self):
        self.assertEqual(read_frame(io.BytesIO(b'{}\n'), 3), b'{}\n')
        for data in (b'', b'{}', b'123\n'):
            with self.subTest(data=data), self.assertRaises(ValueError):
                read_frame(io.BytesIO(data), 3)

    def test_upstream_failure_never_consumes_the_one_shot_loss(self):
        loss = ResponseLoss()
        loss.arm('codex')
        key = str(uuid4())
        request = request_frame({'product': {'op': 'create_managed', 'key': key,
                                             'workspace_id': str(uuid4()), 'profile': 'codex'}})
        upstream = Path('/private/tmp/recorded-agent.sock')
        with patch('w1_recovery.exchange', side_effect=TimeoutError) as exchange:
            with self.assertRaises(TimeoutError):
                loss.relay(io.BytesIO(request), upstream)
        exchange.assert_called_once_with(upstream, request)
        self.assertEqual(loss.profile, 'codex')
        self.assertEqual(loss.drops, [])
        self.assertEqual(loss.count('create_managed', key), 1)


    @contextmanager
    def relay_fixture(self):
        with tempfile.TemporaryDirectory(dir=Path('/tmp').resolve()) as directory:
            state = Path(directory)
            client = state / 'c'
            client.mkdir(mode=0o700)
            path = state / 'agent.sock'
            loss = ResponseLoss()
            with closing(socket.socket(socket.AF_UNIX)) as upstream:
                upstream.bind(str(path))
                path.chmod(0o600)
                upstream.listen()
                upstream.settimeout(2)
                with w1_recovery.agent_proxy(SimpleNamespace(state=state), client, loss):
                    yield upstream, client / 'agent.sock', loss

    def relay_exchange(self, upstream, path, request, response=None, write_failure=False):
        request_bytes = request_frame(request) if isinstance(request, dict) else request
        result = {}

        def serve():
            try:
                with closing(upstream.accept()[0]) as peer:
                    peer.settimeout(2)
                    with peer.makefile('rb') as incoming:
                        result['request'] = read_frame(incoming, w1_recovery.REQUEST_LIMIT)
                    peer.sendall(response)
            except (OSError, ValueError) as error:
                result['error'] = error

        worker = None
        if response is not None:
            worker = threading.Thread(target=serve)
            worker.start()
        sendall = socket.socket.sendall

        def write(peer, data):
            if peer.getsockname() == str(path):
                raise BrokenPipeError
            return sendall(peer, data)

        write_scope = patch.object(socket.socket, 'sendall', write) if write_failure else nullcontext()
        try:
            with write_scope, closing(socket.socket(socket.AF_UNIX)) as peer:
                peer.settimeout(3)
                peer.connect(str(path))
                peer.sendall(request_bytes)
                peer.shutdown(socket.SHUT_WR)
                with peer.makefile('rb') as incoming:
                    received = incoming.read()
        finally:
            if worker is not None:
                worker.join(3)
                self.assertFalse(worker.is_alive())
        if worker is not None:
            self.assertNotIn('error', result)
            self.assertEqual(result.get('request'), request_bytes)
        return received

    def forward_inventory(self, upstream, path, rows, pid=42):
        health = request_frame({'result': 'health', 'agent_pid': pid, 'remote_execution_enabled': False})
        inventory = request_frame({'result': 'runtimes', 'runtimes': rows})
        self.assertEqual(self.relay_exchange(upstream, path, {'op': 'health'}, health), health)
        self.assertEqual(self.relay_exchange(upstream, path, {'op': 'list'}, inventory), inventory)

    def assert_picker_hold_after_invalid_exchange(self, op, reply):
        with self.relay_fixture() as (upstream, path, loss):
            rows = [self.runtime()]
            self.forward_inventory(upstream, path, rows)
            decision = loss.cancel_snapshot()
            self.assertEqual((decision['failures'], decision['active'], decision['forwarded_lists']), (0, 0, 1))
            self.assertEqual(self.relay_exchange(upstream, path, {'op': op}, request_frame(reply)), b'')

            for recovered in (False, True):
                if recovered:
                    self.forward_inventory(upstream, path, rows)
                traffic = w1_recovery.picker_traffic(decision, loss.cancel_snapshot())
                self.assertEqual(traffic['connected'], recovered)
                self.assertFalse(traffic['intact'])
                self.assertEqual(set(traffic['deltas'].values()), {0})
                after = traffic['after']
                self.assertEqual(after['forwarded_lists'] - decision['forwarded_lists'], int(recovered))
                self.assertEqual((after['failures'] - decision['failures'], after['active']), (1, 0))

    def test_handler_desktop_inventory_collection_bounds_hold_quiet_cancel(self):
        print(self._testMethodName, flush=True)
        rows = [self.runtime(runtime_id=UUID(int=index)) for index in range(1, 18)]
        for name, invalid in (('exact duplicate', [rows[0], rows[0]]), ('over capacity', rows)):
            with self.subTest(name=name):
                self.assertTrue(all(w1_recovery.runtime_shape(row) for row in invalid))
                self.assert_picker_hold_after_invalid_exchange('list', {'result': 'runtimes', 'runtimes': invalid})

        with self.relay_fixture() as (upstream, path, loss):
            self.forward_inventory(upstream, path, [])
            before = loss.cancel_snapshot()
            self.assertEqual(before['forwarded_lists'], 1)
            self.forward_inventory(upstream, path, rows[:16])
            traffic = w1_recovery.picker_traffic(before, loss.cancel_snapshot())
            self.assertTrue(traffic['connected'] and traffic['intact'])
            self.assertEqual(traffic['after']['forwarded_lists'], 2)
            self.assertEqual(set(traffic['deltas'].values()), {0})

    def test_handler_native_inventory_projection_holds_quiet_cancel(self):
        print(self._testMethodName, flush=True)
        runtime = self.runtime(runtime_id=UUID('abcdefab-cdef-abcd-efab-cdefabcdefab'))
        managed = self.runtime(str(uuid4()))
        no_origin = {key: value for key, value in managed.items() if key != 'origin'}
        invalid_lists = (
            ('canonical duplicate', [runtime, {**runtime, 'runtime_id': runtime['runtime_id'].upper()}]),
            ('default legacy with IDs', [no_origin]),
            ('explicit legacy with IDs', [{**managed, 'origin': 'legacy'}]),
            ('only workspace ID', [{**managed, 'operation_id': None}]),
            ('only operation ID', [{**managed, 'workspace_id': None}]),
            ('nil workspace ID', [{**managed, 'workspace_id': UUID(int=0).hex}]),
            ('nil operation ID', [{**managed, 'operation_id': f'{{{UUID(int=0)}}}'}]),
            ('zero pane PID', [{**runtime, 'pane_pid': 0}]),
            ('large pane PID', [{**runtime, 'pane_pid': 2**31 + 1}]),
            ('zero width', [{**runtime, 'width': 0}]),
            ('large width', [{**runtime, 'width': 10_001}]),
            ('zero height', [{**runtime, 'height': 0}]),
            ('large height', [{**runtime, 'height': 10_001}]),
            ('overlong UTF-16 command', [{**runtime, 'command': '😀' * 128 + 'a'}]),
        )
        for name, rows in invalid_lists:
            with self.subTest(name=name):
                self.assertTrue(all(w1_recovery.runtime_shape(row) for row in rows))
                self.assert_picker_hold_after_invalid_exchange('list', {'result': 'runtimes', 'runtimes': rows})

        for field, invalid in (
            ('width', True), ('height', 24.0), ('pane_pid', '42'), ('exited', 0), ('command', 1),
            ('pane_id', '%01'), ('pane_id', '%4294967296'), ('pane_id', '$2'),
            ('tmux_name', runtime['tmux_name'] + '0'),
            ('runtime_id', f"{{{UUID(runtime['runtime_id']).hex}}}"),
            ('runtime_id', 'urn:uuid:' + UUID(runtime['runtime_id']).hex),
            ('runtime_id', runtime['runtime_id'] + '\n'),
        ):
            with self.subTest(field=field, invalid=invalid):
                reply = {'result': 'runtimes', 'runtimes': [{**runtime, field: invalid}]}
                self.assert_picker_hold_after_invalid_exchange('list', reply)

        omitted = {'pane_pid', 'workspace_id', 'operation_id'}
        legacy = {key: value for key, value in self.runtime().items() if key not in omitted | {'origin'}}
        unverified = {**self.runtime(), 'origin': 'managed', 'pane_pid': 1, 'pane_id': '%0',
                      'width': 10_000, 'height': 1}
        unverified_omitted = {key: value for key, value in self.runtime().items() if key not in omitted}
        unverified_omitted['origin'] = 'managed'
        alternate = {**self.runtime(str(uuid4())), 'pane_pid': 2**31, 'pane_id': '%4294967295',
                     'width': 1, 'height': 10_000, 'command': '😀' * 128, 'exited': True}
        alternate.update(
            runtime_id=UUID(alternate['runtime_id']).hex,
            session_id=f"{{{alternate['session_id']}}}",
            workspace_id='UrN:UuId:' + alternate['workspace_id'].upper(),
            operation_id=alternate['operation_id'].upper(),
        )
        with self.relay_fixture() as (upstream, path, loss):
            self.forward_inventory(upstream, path, [runtime, legacy, managed])
            before = loss.cancel_snapshot()
            self.forward_inventory(upstream, path, [unverified, unverified_omitted, alternate])
            traffic = w1_recovery.picker_traffic(before, loss.cancel_snapshot())
            self.assertTrue(traffic['connected'] and traffic['intact'])
            self.assertEqual(traffic['after']['forwarded_lists'], 2)

            request = {'op': 'create', 'session_id': runtime['session_id'], 'runtime_id': runtime['runtime_id'],
                       'cwd': '/fixture', 'argv': ['fixture']}
            created = request_frame({'result': 'created', 'runtime': {**runtime, 'width': 0}})
            self.assertEqual(self.relay_exchange(upstream, path, request, created), created)
            self.assertEqual(loss.cancel_snapshot(), traffic['after'])

    def test_handler_desktop_health_pid_bounds_hold_quiet_cancel(self):
        print(self._testMethodName, flush=True)
        for pid in (0, 2**31 + 1):
            with self.subTest(pid=pid):
                self.assertTrue(w1_recovery.U32(pid))
                self.assert_picker_hold_after_invalid_exchange(
                    'health', {'result': 'health', 'agent_pid': pid, 'remote_execution_enabled': False},
                )
        with self.relay_fixture() as (upstream, path, loss):
            rows = [self.runtime()]
            self.forward_inventory(upstream, path, rows, pid=1)
            before = loss.cancel_snapshot()
            self.assertEqual(before['forwarded_lists'], 1)
            self.forward_inventory(upstream, path, rows, pid=2**31)
            traffic = w1_recovery.picker_traffic(before, loss.cancel_snapshot())
            self.assertTrue(traffic['connected'] and traffic['intact'])
            self.assertEqual(traffic['after']['forwarded_lists'], 2)

    def test_handler_malformed_list_holds_quiet_cancel(self):
        print(self._testMethodName, flush=True)
        runtime = self.runtime()
        inventory = request_frame({'result': 'runtimes', 'runtimes': [runtime]})
        request = {'op': 'list'}
        bad_replies = (
            {'result': 'health', 'agent_pid': 42, 'remote_execution_enabled': False},
            {'result': 'runtimes'},
            {'result': 'runtimes', 'runtimes': {}},
            {'result': 'runtimes', 'runtimes': [None]},
            {'result': 'runtimes', 'runtimes': [{**runtime, 'pane_pid': True}]},
            {'result': 'runtimes', 'runtimes': [{**runtime, 'runtime_id': 'invalid'}]},
            {'product': []},
            {'product': {'result': 'blocked', 'issue': 'invalid_request'}},
            {'result': 'error', 'message': None},
            [],
        )
        for reply in (*map(request_frame, bad_replies), b'{"result":', b'{invalid}\n', b'\xff\n'):
            with self.subTest(reply=reply), self.relay_fixture() as (upstream, path, loss):
                self.assertEqual(self.relay_exchange(upstream, path, request, inventory), inventory)
                before = loss.cancel_snapshot()
                self.assertEqual(self.relay_exchange(upstream, path, request, reply), b'')
                middle = loss.cancel_snapshot()
                self.assertEqual((middle['failures'], middle['active'], middle['forwarded_lists']), (1, 0, 1))
                self.assertEqual(self.relay_exchange(upstream, path, request, inventory), inventory)
                traffic = w1_recovery.picker_traffic(before, loss.cancel_snapshot())
                self.assertTrue(traffic['connected'])
                self.assertFalse(traffic['intact'])
                self.assertEqual(set(traffic['deltas'].values()), {0})
                self.assertEqual(traffic['after']['failures'], 1)

        with self.relay_fixture() as (upstream, path, loss):
            self.assertEqual(self.relay_exchange(upstream, path, request, inventory), inventory)
            before = loss.cancel_snapshot()
            error = request_frame({'result': 'error', 'message': 'fixture unavailable'})
            self.assertEqual(self.relay_exchange(upstream, path, request, error), error)
            self.assertEqual(loss.forwarded('list'), 1)
            self.assertEqual(self.relay_exchange(upstream, path, request, inventory), inventory)
            traffic = w1_recovery.picker_traffic(before, loss.cancel_snapshot())
            self.assertTrue(traffic['connected'] and traffic['intact'])

    def test_handler_validates_product_fields_and_accounts_for_write_failure(self):
        print(self._testMethodName, flush=True)
        workspace, key = str(uuid4()), str(uuid4())
        create = {'product': {'op': 'create_managed', 'key': key, 'workspace_id': workspace, 'profile': 'codex'}}
        status = {'product': {'op': 'create_status', 'key': key}}
        register = {'product': {'op': 'register_workspace', 'locator': '/fixture'}}
        readiness = {'product': {'op': 'workspace_readiness', 'workspace_id': workspace, 'profile': 'codex'}}
        runtime = self.runtime(workspace)
        outcome = {'runtime': runtime, 'setup_issue': None}
        confirmed = {'state': 'confirmed', 'outcome': outcome, 'current': 'live', 'setup_issue': None}
        reply = lambda state: {'product': {'result': 'create', 'state': state}}
        error = {'result': 'error', 'message': 'fixture unavailable'}
        blocked = {'product': {'result': 'blocked', 'issue': 'catalog_unconfigured'}}
        for request, value, valid in (
            (create, error, True), (status, blocked, True),
            (readiness, {'product': {'result': 'ready'}}, True),
            (register, {'product': {'result': 'workspace_registered',
                                     'workspace': {'workspace_id': workspace, 'label': 'fixture'}}}, True),
            (register, {'product': {'result': 'workspace_registered',
                                     'workspace': {'workspace_id': workspace}}}, False),
            (readiness, {'product': {'result': 'blocked', 'issue': 'invented'}}, False),
            (status, {'result': 'error', 'message': []}, False),
            (status, {'result': 'runtimes', 'runtimes': []}, False),
            (create, reply({'state': 'pending', 'reservation': 4}), False),
            (create, reply({**confirmed, 'current': 'running'}), False),
            (create, reply({**confirmed, 'outcome': {**outcome, 'runtime': {**runtime, 'width': '80'}}}), False),
            (create, reply({**confirmed, 'current': 'exited'}), True),
        ):
            with self.subTest(value=value), self.relay_fixture() as (upstream, path, loss):
                loss.arm('codex')
                frame = request_frame(value)
                self.assertEqual(self.relay_exchange(upstream, path, request, frame), frame if valid else b'')
                self.assertEqual((loss.failures, loss.active, loss.profile, loss.drops),
                                 (int(not valid), 0, 'codex', []))

        with self.relay_fixture() as (upstream, path, loss):
            for expected, request in enumerate((
                b'{"op":"list"', b'{invalid}\n', b'[]\n', {'op': 'list', 'extra': True},
                {'product': {'op': 'register_workspace'}},
                {'product': {**readiness['product'], 'profile': 'shell'}},
                {'product': {**status['product'], 'reservation': key}},
                {'product': {**create['product'], 'workspace_id': 1}},
                {'op': 'input', 'runtime_id': workspace, 'token': key, 'sequence': True, 'data': [1]},
            ), 1):
                self.assertEqual(self.relay_exchange(upstream, path, request), b'')
                self.assertEqual((loss.failures, loss.active, loss.forwarded('list')), (expected, 0, 0))
            upstream.setblocking(False)
            with self.assertRaises(BlockingIOError):
                upstream.accept()
            upstream.settimeout(2)

            before = loss.failures
            frame = request_frame({'result': 'runtimes', 'runtimes': []})
            self.assertEqual(self.relay_exchange(upstream, path, {'op': 'list'}, frame, write_failure=True), b'')
            self.assertEqual((loss.failures, loss.active, loss.forwarded('list')), (before + 1, 0, 0))

            loss.arm('codex')
            self.assertEqual(self.relay_exchange(upstream, path, create, request_frame(reply(confirmed))), b'')
            self.assertEqual((loss.failures, loss.active, loss.drops), (before + 1, 0, [{'key': key, 'runtime': runtime}]))
            frame = request_frame(reply(confirmed))
            self.assertEqual(self.relay_exchange(upstream, path, status, frame), frame)
            self.assertEqual(loss.forwarded('create_status', key), 1)


class EvidenceTests(unittest.TestCase):
    def picker_case(self, request=None, recorder_failed=False, connected=True):
        recovery = w1_recovery
        case = {'checks': {}, 'commands': []}
        report = {'cases': {'R3-picker-cancel': case}}
        recorder = None

        @contextmanager
        def proxy(lab, client, loss):
            nonlocal recorder
            recorder = loss
            if connected:
                loss.replies['list', None] += 1
            yield

        def cancel(case, name, prompt):
            case['checks'][name] = True
            if request:
                recorder.request({'product': {'op': request}})
            if recorder_failed:
                recorder.failures += 1

        def wait_for_poll(check, timeout):
            if not check() and connected:
                recorder.replies['list', None] += 1
            ready = check()
            if not ready:
                raise TimeoutError
            return ready

        with tempfile.TemporaryDirectory() as directory:
            lab = SimpleNamespace(state=Path(directory), tmux='/tmux')
            with (patch.object(recovery, 'agent_proxy', proxy),
                  patch.object(recovery, 'app_binary', return_value=Path('/app')),
                  patch.object(recovery.subprocess, 'Popen') as launch,
                  patch.object(recovery, 'observe', side_effect=cancel),
                  patch.object(recovery, 'wait_for', side_effect=wait_for_poll),
                  patch.object(recovery, 'input', return_value='', create=True),
                  patch.object(recovery, 'pending_records', return_value={}),
                  patch.object(recovery, 'inventory', return_value={}),
                  patch.object(recovery, 'catalog_snapshot', return_value={'workspace_ids': []})):
                launch.return_value.poll.return_value = None
                recovery.cancel_picker(report, lab)
            self.assertNotEqual(launch.call_args.kwargs['env']['RS_STATE_DIR'], str(lab.state))
        return case

    def test_picker_cancel_requires_quiet_recorded_traffic(self):
        print(self._testMethodName, flush=True)
        case = self.picker_case()
        self.assertEqual(case['status'], Verdict.PASS)
        self.assertTrue(case['recorder']['connected'])
        for op in w1_recovery.CANCEL_OPERATIONS:
            with self.subTest(op=op):
                case = self.picker_case(request=op)
                self.assertEqual(case['status'], Verdict.FAIL)
                self.assertEqual(case['recorder']['deltas'][op], 1)
                self.assertTrue(case['checks']['no_effect'])

    def test_picker_cancel_holds_on_recorder_failure(self):
        print(self._testMethodName, flush=True)
        failed = self.picker_case(recorder_failed=True)
        self.assertTrue(failed['checks']['no_effect'])
        for case in (failed, self.picker_case(connected=False)):
            self.assertEqual(case['status'], Verdict.FAIL)
            self.assertFalse(case['checks']['no_requests'])

    def test_repeated_kingston_runs_create_distinct_reported_workspaces(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            existing = root / 'owner file'
            existing.write_text('keep', encoding='utf-8')
            first_report, second_report = {}, {}
            with patch('w1_recovery.workspace_on_kingston', return_value=root):
                first = prepare_kingston_workspace(first_report, root)
                (first / 'prior run').touch()
                second = prepare_kingston_workspace(second_report, root)

            self.assertNotEqual(first, second)
            self.assertEqual(existing.read_text(encoding='utf-8'), 'keep')
            self.assertTrue((first / 'prior run').is_file())
            self.assertEqual(list(second.iterdir()), [])
            for folder, report in ((first, first_report), (second, second_report)):
                self.assertEqual(folder.parent, root)
                self.assertIn(' ', folder.name)
                self.assertFalse(folder.name.isascii())
                metadata = folder.stat()
                self.assertEqual(stat.S_IMODE(metadata.st_mode), 0o700)
                self.assertEqual(report['kingston'], {
                    'path': str(folder), 'inode': metadata.st_ino, 'device': metadata.st_dev,
                })

    def test_catalog_snapshot_decodes_the_persisted_profile(self):
        with tempfile.TemporaryDirectory() as directory:
            catalog = Path(directory).resolve()
            db = catalog / runtime_lab.CATALOG_DB
            with closing(sqlite3.connect(db)) as connection, connection:
                connection.executescript('''
                    CREATE TABLE workspaces (id TEXT);
                    CREATE TABLE sessions (id TEXT, workspace_id TEXT);
                    CREATE TABLE runtime_attempts
                        (id TEXT, session_id TEXT, target TEXT, nonce TEXT, generation TEXT, policy TEXT);
                    CREATE TABLE create_operations
                        (key TEXT, id TEXT, stage TEXT, profile TEXT, runtime_id TEXT);
                    INSERT INTO workspaces VALUES ('workspace');
                    INSERT INTO sessions VALUES ('session', 'workspace');
                    INSERT INTO create_operations VALUES ('key', 'operation', 'finished', '"codex"', 'runtime');
                ''')
                policy = json.dumps({'workspace': {'canonical': '/project', 'device': 1, 'inode': 2},
                                     'executable': '/bin/codex', 'executable_identity': [1, 3]})
                connection.execute('INSERT INTO runtime_attempts VALUES (?, ?, ?, ?, ?, ?)',
                                   ('runtime', 'session', 'target', 'nonce', '{"pid": 1}', policy))
            db.chmod(0o600)
            (catalog / 'catalog.provision').touch(mode=0o600)
            with patch('w1_recovery.catalog_message', return_value='local catalog ready (fixture)'):
                snapshot = catalog_snapshot(SimpleNamespace(catalog=catalog, state=catalog))
            self.assertEqual(snapshot['operations']['key']['profile'], 'codex')
            self.assertEqual(snapshot['operations']['key']['runtime_id'], 'runtime')

    def test_retained_managed_pane_uses_its_launch_stamp_when_current_pid_is_missing(self):
        runtime = {'tmux_name': 'target', 'pane_id': '%2', 'pane_pid': None, 'origin': 'managed'}
        nonce = str(uuid4())
        row = f'target\t$0\t@1\t%2\t\t1\t{nonce}:$0:@1:%2:42\t/project\t'
        with patch('w1_recovery.tmux_text', return_value=row):
            observed = pane(None, runtime)
        self.assertTrue(observed['dead'])
        self.assertIsNone(observed['pid'])
        self.assertEqual(observed['launch_pid'], 42)
        self.assertEqual(observed['nonce'], nonce)
        with patch('w1_recovery.tmux_text', return_value=row.replace('%2:42', '%3:42')):
            with self.assertRaisesRegex(RuntimeError, 'launch stamp did not match'):
                pane(None, runtime)

    def test_each_case_requires_all_observations_and_explicit_verdicts(self):
        for case, checks in CASE_CHECKS.items():
            with self.subTest(case=case):
                complete = dict.fromkeys(checks, True)
                self.assertEqual(case_status(case, complete), Verdict.PASS)
                incomplete = complete.copy()
                incomplete.pop(checks[0])
                self.assertEqual(case_status(case, incomplete), Verdict.UNRUN)
                self.assertEqual(case_status(case, {**complete, checks[0]: False}), Verdict.FAIL)
                self.assertEqual(case_status(case, {**complete, checks[0]: 1}), Verdict.UNRUN)


class PressureNativeTests(unittest.TestCase):
    def setUp(self):
        self.pressure = w1_terminal_pressure
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.state = Path(scratch.name)
        self.anchor = {'mono_ns': 100_000, 'wall_us': 100}
        self.uid = lambda value: str(UUID(int=value))
        self.args_target = {'runtime_id': self.uid(1), 'generation': self.uid(2), 'session': '$1',
                            'window': '@1', 'pane': '%1', 'pid': 506, 'process_start': 1}
        self.grid = {'cols': 80, 'rows': 24}
        self.epoch = {'agent': self.uid(3), 'owner': self.uid(4)}
        self.pause = {'owner_view': self.uid(5), 'owner_renderer': self.uid(6),
                      'victim_view': self.uid(7), 'victim_renderer': self.uid(8)}
        self.replacement = self.uid(9)
        self.renderer = self.uid(10)
        self.run_id = self.uid(11)
        self.original = {}
        for role, pid in (('owner', 400), ('victim', 401)):
            path = self.state / f'{role}.log'
            path.touch()
            self.original[role] = {'pid': pid, 'started': 'Thu Nov 26 13:57:46 2026', 'role': role,
                                   'stderr': str(path), 'build': {'bundle_id': role}}

    def grid_record(self, view, renderer, revision, at):
        status = {'terminal': {'target': self.args_target, 'view_id': view, 'agent_epoch': self.epoch['agent']},
                  'epoch': self.epoch, 'observation': 'observed',
                  'provider': {'size': self.grid, 'sampled_at_us': at},
                  'committed': {'size': self.grid, 'revision': revision,
                                'intent': {'epoch': self.epoch, 'id': self.uid(20)}}}
        return {'event': {'status': status, 'bridge_pty': status['provider'], 'client': status['provider']},
                'renderer': {'renderer_id': renderer, 'epoch': self.epoch, 'revision': revision,
                             'size': self.grid, 'focused': True}}

    def test_pressure_attachment_must_follow_closure(self):
        print(self._testMethodName, flush=True)
        pressure = self.pressure
        witness = {'retirement': {'times_ns': {'full': 120_000}},
                   'workload': {'started_ns': 110_000, 'finished_ns': 130_000, 'bursts': 750},
                   'owner_convergence': {'grid': self.grid_record(self.pause['owner_view'],
                                                                  self.pause['owner_renderer'], 1, 122)}}
        recovery = {'resumed_at_ns': 150_000, 'resumed_controls': True,
                    'closure': {'view_id': self.pause['victim_view'], 'at_ns': 160_000, 'closed': True,
                                'writable': False, 'data_empty': True, 'status_null': True, 'geometry_null': True,
                                'reason': 'overflow', 'high_water': 16},
                    'redraw': {'view_id': self.replacement, 'renderer_id': self.renderer, 'at_ns': 180_000,
                               'marker': pressure.MARKERS['native'], 'revision': 1, 'size': self.grid,
                               'automatic': True, 'old_renderer_disposed': True, 'tab_preserved': True,
                               'old_work_absent': True},
                    'progress': {'before_ns': 113_000, 'after_ns': 124_000, 'first_burst': 1, 'last_burst': 9,
                                 'view_id': self.pause['owner_view'], 'renderer_id': self.pause['owner_renderer']}}
        victim = pressure.Trace(self.state / 'victim.log')
        victim.failures = [{'view_id': self.pause['victim_view'], 'stage': stage, 'reason': 'stream_closed'}
                           for stage in ('read', 'renderer_detach')]
        victim.grids.append(self.grid_record(self.replacement, self.renderer, 2, 200))
        attached = {'pid': 501, 'at_us': 170}
        victim.pressure[self.replacement] = {'attached': attached}
        args = (self.args_target, self.pause, witness, victim, self.anchor, 140_000, {'pid': 501})
        self.assertEqual(pressure.validate_replacement(recovery, *args), victim.grids[0])
        bad = deepcopy(recovery)
        attached['at_us'] = 154
        bad['redraw']['at_ns'] = 156_000
        with self.assertRaisesRegex(pressure.ProofError, 'does not follow actual victim closure'):
            pressure.validate_replacement(bad, *args)

    def captured(self, role):
        pressure = self.pressure
        writer = role == 'owner'
        view, renderer = self.pause[role + '_view'], self.pause[role + '_renderer']
        trace = pressure.Trace(Path(self.original[role]['stderr']))
        trace.pressure[view] = {'attached': {'pid': 501, 'at_us': 105}}
        grid = self.grid_record(view, renderer, 1, 110)
        trace.grids.append(grid)
        trace._retain_provider(grid['event']['status'])
        capture = {'run_id': self.run_id, 'capture_id': self.uid(40 if writer else 41),
                   'inspector_target': self.uid(42 if writer else 43),
                   'inspector_window': 114 if writer else 115, 'window': 'main',
                   'app': self.original[role], 'captured_ns': 115_000, 'view_id': view,
                   'renderer_id': renderer, 'target': self.args_target, 'epoch': self.epoch,
                   'read_sample_us': 110, 'writable': writer, 'grant_sha256': 'a' * 64 if writer else None,
                   'acknowledgement': None if writer else grid['renderer']}
        invocation = {key: value for key, value in capture.items()
                      if key not in {'captured_ns', 'writable', 'acknowledgement'}}
        invocation['clock_ticks'] = [125, 126, 127]
        invocation['calls'] = []
        for index, (command, stage) in enumerate(pressure.RETIRED_CALLS[role]):
            before = 130 + 20 * index
            rejection = {'view_id': view, 'stage': stage, 'command': command, 'reason': 'view_detached',
                         'rejection_id': self.uid((50 if writer else 60) + index),
                         'event_time_us': before * 1000 + 5000}
            invocation['calls'].append({'command': command, 'before_ms': before,
                                        'after_ms': before + 10, 'rejection': rejection})
        gate = {'run_id': self.run_id, 'capture_id': capture['capture_id'],
                'inspector_target': capture['inspector_target'],
                'inspector_window': capture['inspector_window'], 'issued_ns': 120_000_000, 'log_cursor': 0}
        return trace, capture, gate, invocation, grid

    def trace_line(self, kind, value):
        return self.pressure.PREFIX + kind + b' ' + json.dumps(value).encode() + b'\n'

    def invocation_anchor(self):
        return {'wall_ns': 100_000_000, 'before_ns': 99_999_750, 'after_ns': 100_000_250}

    def record_rejections(self, trace, items):
        trace.path.write_bytes(b''.join(self.trace_line(b'REJECTION', item) for item in items))
        return self.pressure.Trace(trace.path).read()

    def test_pressure_operator_native_input_capture(self):
        print(self._testMethodName, flush=True)
        pressure = self.pressure
        guide = pressure.NATIVE_INSTRUCTIONS
        for literal in ('chunk.paste_incarnation', 'const viewId = view',
                        '"terminal_input", { viewId, incarnation, data, paste: false }',
                        '"terminal_read", { viewId }',
                        '"terminal_acknowledge_grid", { viewId, acknowledgement }',
                        'await invoke(command, { ...args, diagnostic: true })',
                        'calls: [input, followup]', 'const before_ms = Date.now()',
                        'victim-captured before pausing', 'owner-captured before closing',
                        'RS-SPECTATOR-STALE', 'RS-WRITER-STALE', pressure.SYNTHETIC_INCARNATION):
            self.assertIn(literal, guide)
        tauri = (ROOT / 'apps/client/src-tauri/src/lib.rs').read_text()
        for command in ('terminal_input', 'terminal_read', 'terminal_acknowledge_grid'):
            body = tauri.split(f'pub async fn {command}(', 1)[1].split('#[tauri::command]', 1)[0]
            self.assertIn('diagnostic: Option<bool>', body)
            self.assertIn('Result<', body)
            self.assertIn('TerminalInputError>', body)
            self.assertRegex(body, r'terminal_rejection\(\s*error,\s*diagnostic,')
            self.assertIn('RS_W1_RESIZE_REJECTION', body)
        self.assertIn('cfg!(debug_assertions) && std::env::var("RS_W1_RESIZE_TRACE")', tauri)

        used = set()
        for role in ('victim', 'owner'):
            trace, capture, gate, invocation, grid = self.captured(role)
            args = (self.run_id, role, self.original[role], trace, self.args_target, self.anchor, 100_000, 120_000,
                    (capture['view_id'], capture['renderer_id']))
            self.assertEqual(pressure.validate_capture(capture, *args), grid)
            rejections = [call['rejection'] for call in invocation['calls']]
            with patch.object(pressure, 'ns', return_value=200_000_000):
                logged = self.record_rejections(trace, rejections)
            matched = pressure.validate_invocation(invocation, capture, gate, logged, 180_000_000,
                                                   self.invocation_anchor(), used, role)
            self.assertEqual([item['rejection'] for item in matched], rejections)
            for variant in ({**capture, 'view_id': self.replacement},
                            {key: value for key, value in capture.items() if key != 'grant_sha256'},
                            {**capture, 'grant_sha256': 'b' * 64 if role == 'victim' else None}):
                with self.assertRaises(pressure.ProofError):
                    pressure.validate_capture(variant, *args)
            trace.pressure[capture['view_id']]['detached'] = {'at_us': 114, 'pid': 501}
            with self.assertRaises(pressure.ProofError):
                pressure.validate_capture(capture, *args)
        self.assertEqual(len(used), 4)
        ledger = self.state / 'pressure-input.bin'
        expected = pressure.BRIDGE_INPUT + pressure.NATIVE_INPUT
        ledger.write_bytes(expected)
        self.assertEqual(pressure.ledger(self.state, expected), {'exact': True, 'fresh_count': 1, 'stale_count': 0})
        for wrong in (*pressure.STALE_INPUT.values(), pressure.NATIVE_INPUT):
            ledger.write_bytes(expected + wrong)
            with self.assertRaisesRegex(pressure.ProofError, 'input mismatch or replay'):
                pressure.ledger(self.state, expected)

    def test_pressure_rejects_cross_call_and_unprovable_native_rejections(self):
        print(self._testMethodName, flush=True)
        pressure = self.pressure
        owner, capture, gate, invocation, _ = self.captured('owner')
        victim, _, _, _, _ = self.captured('victim')
        native = [call['rejection'] for call in invocation['calls']]
        anchor = self.invocation_anchor()

        def validate(record, trace=None, used=None, scope=gate, clock=anchor):
            return pressure.validate_invocation(record, capture, scope, trace or owner, 180_000_000,
                                                clock, set() if used is None else used, 'owner')

        for event_us in (123500, 130999, 141000, 155000):
            with self.subTest(event_us=event_us), self.assertRaises(pressure.ProofError):
                changed = deepcopy(invocation)
                changed['calls'][0]['rejection']['event_time_us'] = event_us
                validate(changed, self.record_rejections(owner, [c['rejection'] for c in changed['calls']]))

        owner = self.record_rejections(owner, native)
        other_app = self.record_rejections(victim, native)
        for change in ({'app': self.original['victim']},
                       {'app': {**capture['app'], 'started': 'Thu Nov 26 13:57:47 2026'}},
                       {'inspector_target': self.uid(21)}, {'inspector_window': 999},
                       {'capture_id': self.uid(22)}, {'view_id': self.pause['victim_view']},
                       {'grant_sha256': 'b' * 64}, {'clock_ticks': [125, 127, 129]},
                       {'calls': list(reversed(invocation['calls']))},
                       {'calls': [invocation['calls'][0], {**invocation['calls'][1], 'before_ms': 141}]}):
            with self.subTest(change=change), self.assertRaises(pressure.ProofError):
                validate({**invocation, **change})
        for field, value in (('rejection_id', native[1]['rejection_id']), ('stage', 'read'),
                             ('command', 'terminal_read'), ('reason', 'stale_input'),
                             ('event_time_us', native[0]['event_time_us'] + 1)):
            changed = deepcopy(invocation)
            changed['calls'][0]['rejection'][field] = value
            with self.subTest(field=field), self.assertRaises(pressure.ProofError):
                validate(changed)
        for rejection in ('View detached', {}, {**native[0], 'token': 'unexpected'}):
            changed = deepcopy(invocation)
            changed['calls'][0]['rejection'] = rejection
            with self.subTest(rejection=rejection), self.assertRaises(pressure.ProofError):
                validate(changed)
        swapped = deepcopy(invocation)
        for call, item in zip(swapped['calls'], reversed(native)):
            call['rejection']['rejection_id'] = item['rejection_id']
        with self.assertRaises(pressure.ProofError):
            validate(swapped)
        with self.assertRaises(pressure.ProofError):
            validate(invocation, other_app)
        with self.assertRaises(pressure.ProofError):
            validate(invocation, used={native[0]['rejection_id']})
        with self.assertRaises(pressure.ProofError):
            validate(invocation, scope={**gate, 'log_cursor': owner.rejections[0][2]})
        with self.assertRaises(pressure.ProofError):
            validate(invocation, clock={**anchor, 'after_ns': 102_000_000})
        with self.assertRaises(pressure.ProofError):
            pressure.corroborate_clock(anchor, {**anchor, 'wall_ns': 102_000_000})

        unrelated = {**native[0], 'rejection_id': self.uid(99), 'event_time_us': 128000}
        for bad in ([native[1]], [native[0], native[0], native[1]], [unrelated, *native], list(reversed(native))):
            with self.subTest(bad=bad), self.assertRaises(pressure.ProofError):
                validate(invocation, self.record_rejections(owner, bad))

    def test_pressure_stage_and_final_records_catch_up_both_real_traces(self):
        print(self._testMethodName, flush=True)
        pressure = self.pressure
        traces, captures = [], []
        for role in ('owner', 'victim'):
            trace, capture, _, _, grid = self.captured(role)
            trace.path.write_bytes(
                b'ordinary stderr\n' * 8200
                + self.trace_line(b'PRESSURE', {'event': 'attached', 'view_id': capture['view_id'],
                                                 'pid': 501, 'at_us': 105})
                + self.trace_line(b'GRID', grid)
                + self.trace_line(b'PROVIDER', grid['event']['status']))
            traces.append(pressure.Trace(trace.path))
            captures.append(capture)

        def stage_record():
            for trace in traces:
                self.assertFalse(trace.grids)
                self.assertGreater(trace.offset, 0)
            return captures[0]

        captured, received = pressure.wait_native_record(stage_record, traces, 2, 'missing stage')
        for trace, capture in zip(traces, captures):
            role = capture['app']['role']
            result = pressure.validate_capture(capture, self.run_id, role, capture['app'], trace,
                                               self.args_target, self.anchor, 100_000, received)
            self.assertEqual(result['renderer']['renderer_id'], capture['renderer_id'])
        self.assertEqual(captured, captures[0])

        records = {}
        inventory = []
        for index, profile in enumerate(('claude_code', 'codex'), 1):
            target = {**self.args_target, 'runtime_id': self.uid(70 + index), 'pid': 600 + index}
            inventory.append({'runtime_id': target['runtime_id'], 'pane_pid': target['pid'],
                              'exited': False, 'origin': 'managed'})
            record = {'target': target, 'paused_ns': 240_000, 'startup_ns': 250_000, 'edit_ns': 260_000,
                      'resumed_ns': 270_000, 'prompts_submitted': 0, 'inspector': 'immediate_pause_requested',
                      'continued': True, 'trust_auth_unchanged': True, 'startup_visible': True,
                      'unsent_edit_visible': True}
            for role, trace, offset in zip(('owner', 'victim'), traces, (0, 10)):
                view, renderer = self.uid(80 + offset + index), self.uid(100 + offset + index)
                grid = self.grid_record(view, renderer, 1, 220 + index)
                grid['event']['status']['terminal']['target'] = target
                with trace.path.open('ab') as stream:
                    stream.write(b'ordinary stderr\n' * 8200 + self.trace_line(b'GRID', grid)
                                 + self.trace_line(b'PROVIDER', grid['event']['status']))
                record[f'{role}_view'], record[f'{role}_renderer'] = view, renderer
            records[profile] = record
        evidence_path = self.state / 'final.json'
        evidence_path.write_bytes(request_frame(records))
        evidence_path.chmod(0o600)
        final, _ = pressure.wait_native_record(lambda: pressure.read_if_present(evidence_path),
                                               traces, 2, 'missing final')
        lab = SimpleNamespace(call=lambda op: {'runtimes': inventory})
        with (patch.object(pressure, 'tmux_target'),
              patch.object(pressure, 'managed_executable', return_value='/fixture/provider'),
              patch.object(pressure, 'output', return_value='1.2.3'),
              patch.object(pressure, 'digest', return_value='fixture digest'),
              patch.object(pressure, 'live_process', return_value={'pid': 600})):
            results = pressure.validate_providers(final, lab, self.anchor, *traces, 210_000, self.epoch['agent'])
        self.assertEqual(set(results), {'claude_code', 'codex'})
        for trace in traces:
            self.assertEqual(trace.offset, trace.path.stat().st_size)
            self.assertEqual({sample['provider']['sampled_at_us'] for sample in trace.provider_samples()},
                             {110, 221, 222})

    def test_pressure_catch_up_holds_incomplete_or_unavailable_boundaries(self):
        print(self._testMethodName, flush=True)
        pressure = self.pressure
        path = self.state / 'partial.log'
        path.write_bytes(b'complete\npartial')
        trace = pressure.Trace(path)
        record, _ = pressure.wait_native_record(lambda: {'stage': 1}, (trace,), 1, 'missing')
        self.assertEqual(record, {'stage': 1})
        self.assertEqual(trace.offset, len(b'complete\n'))
        self.assertEqual(trace.grids, [])
        with path.open('ab') as stream:
            stream.write(b' complete\n')
        pressure.catch_up_role_logs((trace,), time.monotonic() + 1)
        self.assertEqual(trace.offset, path.stat().st_size)

        with patch.object(pressure.time, 'monotonic', side_effect=(0, 2)):
            with self.assertRaisesRegex(pressure.ProofError, 'deadline'):
                pressure.catch_up_role_logs((trace,), 1)

        with path.open('ab') as stream:
            stream.write(b'x' * pressure.MAX_RECORD)
        with self.assertRaisesRegex(pressure.ProofError, 'oversized'):
            pressure.catch_up_role_logs((trace,), time.monotonic() + 1)

        with path.open('rb') as stream:
            closed = pressure.Trace(path, stream)
        with self.assertRaises(pressure.ProofError):
            pressure.catch_up_role_logs((closed,), time.monotonic() + 1)

        replacement = self.state / 'replacement.log'
        replacement.write_bytes(b'replaced\n')
        replacement.replace(path)
        with self.assertRaisesRegex(pressure.ProofError, 'stream changed'):
            pressure.catch_up_role_logs((trace,), time.monotonic() + 1)


class PressureBuildTests(unittest.TestCase):
    def setUp(self):
        self.pressure = w1_terminal_pressure
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        directory = Path(scratch.name).resolve()
        self.checkout = directory / 'checkout'
        self.checkout.mkdir()
        self.outputs = directory / 'private'
        self.outputs.mkdir(mode=0o700)
        self.commit, self.tree = '1' * 40, '2' * 40
        self.status = ''
        self.builds = []
        self.fail_build = False
        self.tools = {name: {'path': f'/mock/{name}', 'version': 'fixture'}
                      for name in ('git', 'make', 'cargo', 'rustc', 'node', 'npm', 'codesign')}
        for name in self.pressure.LOCKFILES:
            (self.checkout / name).write_bytes(b'locked')
        self.start_patch(self.pressure, 'ROOT', self.checkout)
        self.start_patch(self.pressure, 'run_command', side_effect=self.command)
        self.start_patch(self.pressure, 'build_tools', side_effect=lambda *args: deepcopy(self.tools))
        self.start_patch(self.pressure, 'tauri_tool', return_value={'path': '/mock/tauri', 'version': 'fixture'})
        self.start_patch(self.pressure, 'signed_app', side_effect=self.signed_app)
        self.start_patch(self.pressure.sys, 'platform', 'darwin')

    def start_patch(self, obj, field, *args, **kwargs):
        mocked = patch.object(obj, field, *args, **kwargs)
        self.addCleanup(mocked.stop)
        return mocked.start()

    def command(self, *args, **kwargs):
        if args[0] == '/usr/bin/git':
            if '--show-toplevel' in args:
                output = str(self.checkout)
            elif 'rev-parse' in args:
                output = f'{self.commit}\n{self.tree}\n'
            elif 'ls-files' in args:
                output = 'H Cargo.lock\0H crates/host-agent/src/engine.rs\0'
            elif 'status' in args:
                output = self.status
            else:
                self.fail(f'Unexpected fixture git arguments {args}')
            return subprocess.CompletedProcess(args, 0, output, '')
        if args[:2] == ('/usr/bin/codesign', '-d'):
            return subprocess.CompletedProcess(args, 1, '', '')
        self.builds.append(tuple(map(str, args)))
        if self.fail_build:
            raise self.pressure.ProofError('controlled fixture build failed')
        if args[:2] == ('/mock/cargo', 'build'):
            for path in (self.pressure.HOST_PATH, self.pressure.PROBE_PATH):
                binary = self.checkout / path
                binary.parent.mkdir(parents=True, exist_ok=True)
                binary.write_bytes(str(path).encode())
                binary.chmod(0o755)
        if args[:2] == ('/mock/npm', 'run'):
            bundle = self.checkout / self.pressure.APP_PATH
            binary = bundle / 'Contents/MacOS/client'
            binary.parent.mkdir(parents=True, exist_ok=True)
            binary.write_bytes(b'compiled app')
            binary.chmod(0o755)
            (bundle / 'Contents/Info.plist').write_bytes(plistlib.dumps({
                'CFBundleExecutable': 'client', 'CFBundleIdentifier': 'dev.test',
            }))
        return subprocess.CompletedProcess(args, 0, '', '')

    def signed_app(self, bundle, scratch, role):
        metadata = plistlib.loads((bundle / 'Contents/Info.plist').read_bytes())
        binary = bundle / 'Contents/MacOS' / metadata['CFBundleExecutable']
        checksum = self.pressure.digest(binary)
        return binary, {'bundle_id': metadata['CFBundleIdentifier'], 'binary_sha256': checksum,
                        'code_hash': checksum[:40], 'unsigned_sha256': checksum}

    def produce(self, mode, suffix=''):
        self.builds.clear()
        shutil.rmtree(self.checkout / 'target', ignore_errors=True)
        record = self.outputs / f'{mode}{suffix}.json'
        owner = self.outputs / f'{mode}{suffix}-owner.app'
        victim = self.outputs / f'{mode}{suffix}-victim.app'
        paths = [] if mode == 'bridge' else ['--owner-app', str(owner)]
        if mode == 'installed':
            paths.extend(('--victim-app', str(victim)))
        argv = ['--controlled-build', mode, '--build-record', str(record), *paths]
        with redirect_stdout(io.StringIO()) as completion, redirect_stderr(io.StringIO()):
            result = self.pressure.main(argv)
        if self.fail_build:
            self.assertNotEqual(result, 0)
            self.assertFalse(record.exists())
            self.assertNotIn('BUILD_RECORD_SHA256=', completion.getvalue())
            return None
        self.assertEqual(result, 0)
        self.assertEqual(stat.S_IMODE(record.stat().st_mode), 0o600)
        prefix, checksum = completion.getvalue().strip().split('=')
        self.assertEqual(prefix, 'BUILD_RECORD_SHA256')
        self.assertEqual(self.pressure.digest(record), checksum)
        receipt = self.pressure.capture_json(record)
        self.assertEqual(receipt['mode'], mode)
        self.assertEqual(self.builds, [tuple(argv) for argv in receipt['argv']])
        self.assertEqual(bool([argv for argv in self.builds if argv[:2] == ('/mock/npm', 'ci')]), mode != 'bridge')
        self.assertEqual(bool([argv for argv in self.builds if argv[-2:] == ('--', '--locked')]), mode != 'bridge')
        if mode == 'installed':
            paths.extend(('--native-evidence', str(self.outputs / f'{mode}{suffix}-native.json')))
        elif mode == 'paste':
            paths.extend(('--paste', '--paste-native', str(self.outputs / f'{mode}-native.json')))
        return ['--build-record', str(record), '--build-record-sha256', checksum, *paths], receipt

    def successful_pressure(self, options, report, build):
        report.update(bridge={'outcome': 'passed'}, native={'outcome': 'passed'}, cleanup='confirmed')

    def successful_paste(self, options, report, build, stack, observe):
        observe(42, 51, {42: ('fixture', None), 43: ('host', None)})
        report.update(paste={'outcome': 'passed'}, cleanup='confirmed')

    def consume(self, argv, pressure_work=None, paste_work=None, observed=None):
        pressure = self.pressure
        report = self.outputs / f'report-{uuid4().hex}.json'
        expected = {
            42: self.checkout / pressure.PROBE_PATH, 43: self.checkout / pressure.HOST_PATH,
            51: self.outputs / 'paste-owner.app/Contents/MacOS/client',
        }
        calls = []

        def running(pid, artifact, original=None):
            calls.append(pid)
            path = (observed or {}).get((pid, calls.count(pid)), expected[pid])
            if str(path) != artifact['path'] or pressure.file_identity(path) != artifact:
                raise pressure.ProvenanceHold('running fixture or WebView did not match')
            return {'pid': pid, 'started': 'original'}

        with (patch.object(pressure, 'prove', side_effect=pressure_work or self.successful_pressure),
              patch.object(pressure, 'paste_workload',
                           side_effect=pressure.paste_workload if paste_work is False
                           else paste_work or self.successful_paste),
              patch.object(pressure, 'running_artifact', side_effect=running),
              redirect_stdout(io.StringIO())):
            result = pressure.main([*argv, '--report', str(report)])
        return result, json.loads(report.read_text())

    def require_pass(self, argv):
        code, report = self.consume(argv)
        self.assertEqual(code, 0, report)
        self.assertTrue(report['passed'])

    def require_hold(self, argv, **kwargs):
        code, report = self.consume(argv, **kwargs)
        self.assertNotEqual(code, 0)
        self.assertFalse(report['passed'])
        self.assertEqual(report['provenance'], 'hold', report)

    def test_pressure_rejects_dirty_build_inputs(self):
        print(self._testMethodName, flush=True)
        for mode in self.pressure.BUILD_MODES:
            with self.subTest(mode=mode):
                argv, _ = self.produce(mode)
                self.require_pass(argv)
                for entry in ('M  ', ' M ', '?? '):
                    with self.subTest(status=entry):
                        self.status = entry + 'crates/host-agent/src/engine.rs\0'
                        self.require_hold(argv)
                self.status = ''
                self.commit = 'a' * 40
                self.require_hold(argv)
                self.commit = '1' * 40
                self.tree = 'b' * 40
                self.require_hold(argv)
                self.tree = '2' * 40

                def dirty_pressure(options, report, build):
                    self.successful_pressure(options, report, build)
                    self.status = ' M crates/host-agent/src/engine.rs\0'

                def dirty_paste(options, report, build, stack, observe):
                    self.successful_paste(options, report, build, stack, observe)
                    self.status = ' M crates/host-agent/src/engine.rs\0'

                self.require_hold(argv, pressure_work=dirty_pressure, paste_work=dirty_paste)
                self.status = ''
        cache = self.checkout / '.cache'
        cache.mkdir()
        manifest = cache / 'runtime-lab-w1-pressure-012345abcdef.json'
        recovery = manifest.with_suffix('.recovery.json')
        recovery.touch()
        self.status = '!! .cache/\0'
        lab = SimpleNamespace(manifest=manifest, recovery_path=recovery)
        self.pressure.source_snapshot(labs=(lab,))
        with self.assertRaises(self.pressure.ProofError):
            self.pressure.source_snapshot()

    def test_pressure_rejects_stale_binary_provenance(self):
        print(self._testMethodName, flush=True)
        for mode in self.pressure.BUILD_MODES:
            with self.subTest(mode=mode):
                argv, receipt = self.produce(mode)
                self.require_pass(argv)
                self.require_hold(argv[2:])
                self.require_hold(argv[:2] + argv[4:])
                self.require_hold(argv[:3] + ['0' * 64] + argv[4:])
                record = self.outputs / f'{mode}.json'
                wrong = self.outputs / f'{mode}-wrong.json'
                checksum = self.pressure.write_json(wrong, {**receipt, 'mode': 'paste' if mode == 'bridge' else 'bridge'})
                self.require_hold([argv[0], str(wrong), argv[2], checksum, *argv[4:]])
                self.require_hold([argv[0], str(record.with_suffix('.missing')), *argv[2:]])
                probe = self.checkout / self.pressure.PROBE_PATH
                if mode == 'paste':
                    self.require_hold(argv, paste_work=False)
                    substituted = self.outputs / 'other-live-executable'
                    substituted.write_bytes(b'elsewhere')
                    for pid, sample in ((42, 1), (42, 2), (43, 2), (51, 1), (51, 2)):
                        with self.subTest(pid=pid, sample=sample):
                            self.require_hold(argv, observed={(pid, sample): substituted})
                else:
                    def changed(options, report, build):
                        self.successful_pressure(options, report, build)
                        original = probe.read_bytes()
                        probe.write_bytes(b'changed during collection')
                        probe.write_bytes(original)

                    if mode == 'bridge':
                        self.require_hold(argv, pressure_work=changed)
                if mode == 'installed':
                    alternative = self.outputs / 'other-signed-copy.app'
                    shutil.copytree(Path(receipt['artifacts']['owner']['path']), alternative)
                    self.require_hold([str(alternative) if value == receipt['artifacts']['owner']['path'] else value
                                       for value in argv])
                    for role in ('owner', 'victim'):
                        signed_argv, signed_build = self.produce(mode, '-' + role)
                        self.require_pass(signed_argv)
                        signed = Path(signed_build['artifacts'][role]['executable']['path'])
                        if role == 'owner':
                            signed.write_bytes(b'changed signed executable')
                            self.require_hold(signed_argv)
                        else:
                            def changed_signed(options, report, build):
                                self.successful_pressure(options, report, build)
                                signed.write_bytes(b'changed signed executable')

                            self.require_hold(signed_argv, pressure_work=changed_signed)
                else:
                    probe.write_bytes(b'stale probe')
                    self.require_hold(argv)

    def test_pressure_failed_controlled_build_has_no_completion_receipt(self):
        self.fail_build = True
        for mode in self.pressure.BUILD_MODES:
            with self.subTest(mode=mode):
                self.produce(mode)


if __name__ == '__main__':
    unittest.main()
