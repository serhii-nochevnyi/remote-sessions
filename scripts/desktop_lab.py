#!/usr/bin/env python3
"""Prepare real local sessions for the native desktop development client."""
import argparse
import json
import os
import plistlib
import shutil
import sys
from pathlib import Path
from runtime_lab import Lab, ROOT, run

MANIFEST = ROOT / '.cache/desktop-lab.json'
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('action', choices=['up', 'run', 'down'])
parser.add_argument('--with-clis', action='store_true')
args = parser.parse_args()
if args.action == 'run':
    record = json.loads(MANIFEST.read_text())
    app = ROOT / 'target/debug/bundle/macos/Remote Sessions.app'
    info = plistlib.loads((app / 'Contents/Info.plist').read_bytes())
    binary = app / 'Contents/MacOS' / info['CFBundleExecutable']
    environment = dict(os.environ, RS_STATE_DIR=record['state_dir'], RS_TMUX_BIN=record['tmux'])
    os.execve(binary, [str(binary)], environment)
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
        probe = lab.state / 'runtime_probe.py'
        shutil.copy2(ROOT / 'tests/integration/runtime_probe.py', probe)
        runtimes = [lab.call('create', '--cwd', lab.state, '--', sys.executable, probe, lab.state/'heartbeat.json', lab.state/'input.txt')['runtime']]
        if args.with_clis:
            for name, options in [
                ('claude', ['--restricted','--permission-mode','manual','--strict-mcp-config','--mcp-config','{"mcpServers":{}}','--tools','','--no-chrome']),
                ('codex', ['--no-daemon','--sandbox','read-only','--ask-for-approval','on-request','-c','mcp_servers={}']),
            ]:
                binary = shutil.which(name)
                if not binary: raise RuntimeError(name + ' is unavailable')
                directory = lab.state / name; directory.mkdir()
                runtimes.append(lab.call('create','--cwd',directory,'--',binary,*options)['runtime'])
        record = {'state_dir':str(lab.state),'tmux':lab.tmux,'targets':lab.loaded,'runtimes':runtimes}
        MANIFEST.write_text(json.dumps(record,indent=2)+'\n'); MANIFEST.chmod(0o600)
        print(json.dumps(record,indent=2))
    except Exception:
        lab.close(); raise
