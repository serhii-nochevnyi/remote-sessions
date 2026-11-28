#!/usr/bin/env python3
"""Prepare real local sessions for the native desktop development client."""
import argparse
import json
import os
import plistlib
import shutil
import stat
import sys
from pathlib import Path

from runtime_lab import Lab, ROOT, W1_ID, W1Record, close_w1, private_path, run

MANIFEST = ROOT / '.cache/desktop-lab.json'
W1_MANIFEST = ROOT / '.cache/desktop-lab-w1.json'
FIXTURE_MANIFEST = ROOT / '.cache/desktop-lab-w1-fixture.json'
APP = ROOT / 'target/debug/bundle/macos/Remote Sessions.app'


def app_binary():
    info = plistlib.loads((APP / 'Contents/Info.plist').read_bytes())
    return APP / 'Contents/MacOS' / info['CFBundleExecutable']


def seed_legacy(lab, *, suffix=''):
    probe = lab.state / ('runtime_probe' + suffix + '.py')
    shutil.copy2(ROOT / 'tests/integration/runtime_probe.py', probe)
    return lab.call('create', '--cwd', lab.state, '--', sys.executable, probe,
                    lab.state / ('heartbeat' + suffix + '.json'),
                    lab.state / ('input' + suffix + '.txt'))['runtime']


def fixture_client_state(lab, name):
    if not name.startswith('client-') or not W1_ID.fullmatch(name[7:]):
        raise ValueError('Expected a W1 fixture client directory name')
    directory = lab.state / name
    private_path(directory, stat.S_ISDIR, 0o700)
    if len(os.fsencode(directory)) > 80:
        raise ValueError('W1 fixture client socket path is too long')
    private_path(directory / 'agent.sock', stat.S_ISSOCK, 0o600)
    tmux = directory / 'tmux.sock'
    expected = lab.state / 'tmux.sock'
    if not tmux.is_symlink() or Path(os.readlink(tmux)) != expected:
        raise ValueError('W1 client tmux.sock must point to the dedicated lab socket')
    private_path(expected, stat.S_ISSOCK, (0o600, 0o700))
    return directory


def start_w1():
    lab = Lab(w1=True, manifest=W1_MANIFEST).start()
    try:
        record = lab.publish_w1_manifest(runtimes=[seed_legacy(lab)])
    except (Exception, KeyboardInterrupt):
        lab.close()
        raise
    print(json.dumps(record, indent=2))


def run_app(state, tmux):
    binary = app_binary()
    environment = dict(os.environ, RS_STATE_DIR=str(state), RS_TMUX_BIN=tmux)
    os.execve(binary, [str(binary)], environment)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['up', 'run', 'down'])
    parser.add_argument('--with-clis', action='store_true')
    parser.add_argument('--w1', action='store_true')
    parser.add_argument('--fixture', action='store_true',
                        help='Select the separate W1 recovery fixture jobs')
    parser.add_argument('--client-state', help='Private client directory name from the recovery harness')
    args = parser.parse_args()
    if args.fixture and (not args.w1 or args.action == 'up'):
        parser.error('--fixture requires run --w1 or down --w1')
    if args.client_state is not None and (args.action != 'run' or not args.fixture):
        parser.error('--client-state requires run --w1 --fixture')
    if args.fixture and args.action == 'run' and args.client_state is None:
        parser.error('Fixture app launches require --client-state from the recovery harness')
    if args.w1:
        if args.with_clis:
            parser.error('--with-clis is for the P0 lab')
        manifest = FIXTURE_MANIFEST if args.fixture else W1_MANIFEST
        if args.action == 'up':
            start_w1()
        elif args.action == 'down':
            close_w1(manifest, fixture=args.fixture)
            print('W1 desktop lab stopped.')
        else:
            lab = W1Record(manifest, fixture=args.fixture)
            for name in lab.targets:
                if not lab.loaded_pid(name):
                    parser.error(f'W1 {name} job is not running')
            state = lab.state
            if args.client_state is not None:
                state = fixture_client_state(lab, args.client_state)
            run_app(state, lab.tmux)
        return

    if args.action == 'run':
        record = json.loads(MANIFEST.read_text())
        run_app(record['state_dir'], record['tmux'])
    elif args.action == 'down':
        record = json.loads(MANIFEST.read_text())
        for target in reversed(record['targets']):
            if not target.startswith(f'gui/{os.getuid()}/dev.remotesessions.p0.'):
                raise SystemExit('Unexpected service target')
            run('launchctl', 'bootout', target)
        MANIFEST.unlink()
        print('Desktop lab stopped; temporary logs retained at ' + record['state_dir'])
    else:
        if MANIFEST.exists():
            raise SystemExit('Desktop lab already recorded; run down first')
        lab = Lab().start()
        try:
            runtimes = [seed_legacy(lab)]
            if args.with_clis:
                for name, options in [
                    ('claude', ['--restricted','--permission-mode','manual','--strict-mcp-config','--mcp-config','{"mcpServers":{}}','--tools','','--no-chrome']),
                    ('codex', ['--no-daemon','--sandbox','read-only','--ask-for-approval','on-request','-c','mcp_servers={}']),
                ]:
                    binary = shutil.which(name)
                    if not binary:
                        raise RuntimeError(name + ' is unavailable')
                    directory = lab.state / name
                    directory.mkdir()
                    runtimes.append(lab.call('create', '--cwd', directory, '--', binary, *options)['runtime'])
            record = {'state_dir': str(lab.state), 'tmux': lab.tmux, 'targets': lab.loaded, 'runtimes': runtimes}
            MANIFEST.write_text(json.dumps(record, indent=2) + '\n')
            MANIFEST.chmod(0o600)
            print(json.dumps(record, indent=2))
        except Exception:
            lab.close()
            raise


if __name__ == '__main__':
    main()
