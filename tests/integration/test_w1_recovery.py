"""Stdlib checks for lab scoping and the native recovery evidence contract."""
import argparse
from contextlib import closing
import io
import json
import os
from pathlib import Path
import plistlib
import shutil
import sqlite3
import stat
import subprocess
import sys
import tempfile
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
                      patch.object(w1_terminal_pressure, 'sample_clock', return_value={})):
                    with self.assertRaises(subprocess.TimeoutExpired):
                        if pressure:
                            w1_terminal_pressure.prove(SimpleNamespace(), report)
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
    def test_drop_only_one_complete_confirmed_create_and_forward_status_inventory(self):
        key = str(uuid4())
        workspace_id = str(uuid4())
        create = {'product': {'op': 'create_managed', 'key': key, 'workspace_id': workspace_id,
                              'profile': 'codex'}}
        runtime = {'operation_id': str(uuid4()), 'runtime_id': str(uuid4()),
                   'workspace_id': workspace_id, 'session_id': str(uuid4()),
                   'origin': 'managed', 'pane_id': '%2', 'pane_pid': 42,
                   'exited': False}
        runtime['tmux_name'] = 'rs_{}_{}'.format(runtime['session_id'].replace('-', ''),
                                                runtime['runtime_id'].replace('-', ''))
        reply = request_frame({'product': {'result': 'create', 'state': {
            'state': 'confirmed', 'current': 'live', 'outcome': {'runtime': runtime}}}})
        failure = request_frame({'product': {'result': 'create', 'state': {
            'state': 'failed', 'issue': 'workspace_changed'}}})
        status = {'product': {'op': 'create_status', 'key': key}}
        inventory = request_frame({'result': 'runtimes', 'runtimes': [runtime]})
        loss = ResponseLoss()
        self.assertEqual(loss.response(create, reply), reply)
        loss.arm('codex')
        self.assertEqual(loss.response(create, failure), failure)
        wrong_target = json.loads(reply)
        wrong_target['product']['state']['outcome']['runtime']['tmux_name'] = 'foreign'
        wrong_target = request_frame(wrong_target)
        self.assertEqual(loss.response(create, wrong_target), wrong_target)
        self.assertEqual(loss.response(status, reply), reply)
        with self.assertRaises(ValueError):
            loss.response(create, reply[:-1])
        self.assertIsNone(loss.response(create, reply))
        self.assertEqual(loss.response(status, reply), reply)
        self.assertEqual(loss.response({'op': 'list'}, inventory), inventory)
        self.assertEqual(loss.response(create, reply), reply)
        self.assertEqual(loss.drops, [{'key': key, 'runtime': runtime}])
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


class EvidenceTests(unittest.TestCase):
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


if __name__ == '__main__':
    unittest.main()
