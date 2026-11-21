#!/usr/bin/env python3
"""Opt-in real Claude/Codex TUI test. No model prompt is submitted."""
import argparse
import base64
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'scripts'))
from runtime_lab import Lab, run, wait_for
from runtime_lifecycle import Viewer

DRAFT = 'P0_DRAFT Привіт 漢字'


def verify(capture_dir):
    capture_dir.mkdir(parents=True, exist_ok=True)
    capture_dir.chmod(0o700)
    report = {'test': 'real CLI TUI compatibility', 'model_prompts_submitted': 0, 'tools': {}, 'checks': []}
    captures = []
    viewers = []
    runtimes = {}
    with Lab() as lab:
        try:
            def check(text):
                report['checks'].append(text)
                print('PASS ' + text, flush=True)

            def snapshot(runtime):
                return lab.call('snapshot', runtime['runtime_id'])['text']

            def inventory(runtime):
                return next(r for r in lab.call('list')['runtimes'] if r['runtime_id'] == runtime['runtime_id'])

            def stable(runtime, before):
                current = inventory(runtime)
                for key in ['session_id', 'runtime_id', 'pane_id', 'pane_pid']:
                    assert current[key] == runtime[key], (key, current, runtime)
                assert not current['exited']
                assert run('ps', '-p', current['pane_pid'], '-o', 'lstart=').stdout.strip() == before
                return current

            def send_literal(runtime, text):
                result = lab.tmux_call('send-keys', '-t', runtime['pane_id'], '-l', '--', text)
                assert result.returncode == 0, result.stderr

            def capture(name, stage, runtime, cols, rows, expected):
                viewer = Viewer(lab, runtime['runtime_id'], width=cols, height=rows)
                viewers.append(viewer)
                deadline, changed, length = time.monotonic() + 5, time.monotonic(), 0
                while time.monotonic() < deadline:
                    viewer.read()
                    assert len(viewer.output) <= 2_097_152, "attachment output exceeds test capture limit"
                    if len(viewer.output) != length:
                        length, changed = len(viewer.output), time.monotonic()
                    if length and time.monotonic() - changed > 0.4:
                        break
                    time.sleep(0.03)
                assert viewer.output, 'empty ordinary PTY attachment'
                content = {'cols': cols, 'rows': rows, 'base64': base64.b64encode(viewer.output).decode(), 'expected': expected}
                result = subprocess.run(['node', str(ROOT / 'tests/integration/render_terminal.mjs')], input=json.dumps(content), capture_output=True, text=True, timeout=10)
                decoded = json.loads(result.stdout)
                assert result.returncode == 0, {'stage':stage,'tool':name,'missing':decoded['missing'],'screen':decoded['text']}
                assert '\ufffd' not in decoded['text'], 'replacement characters in xterm output'
                path = capture_dir / f'{name}-{stage}.json'
                path.write_text(json.dumps({**content, 'name': name, 'stage': stage, 'decoded': decoded}, ensure_ascii=False))
                path.chmod(0o600)
                captures.append({'name': name, 'stage': stage, 'file': str(path), 'cols':cols,'rows':rows})
                viewer.close()
                return {'cols':cols,'rows':rows,'xterm_buffer':decoded['buffer'],'bytes':length,'unicode_draft_visible':True}

            runtimes = {}
            for name, args, marker in [
                ('claude', ['--restricted','--permission-mode','manual','--strict-mcp-config','--mcp-config','{"mcpServers":{}}','--tools','','--no-chrome'], 'Claude Code'),
                ('codex', ['--no-daemon','--sandbox','read-only','--ask-for-approval','on-request','-c','mcp_servers={}'], 'OpenAI Codex'),
            ]:
                binary = shutil.which(name)
                assert binary, f'{name} is not installed'
                cwd = lab.state / name
                cwd.mkdir()
                runtime = lab.call('create','--cwd',cwd,'--',binary,*args)['runtime']
                runtimes[name] = runtime
                def settled_startup():
                    previous, changed, deadline = '', time.monotonic(), time.monotonic() + 30
                    while time.monotonic() < deadline:
                        value = snapshot(runtime)
                        if value != previous:
                            previous, changed = value, time.monotonic()
                        if time.monotonic() - changed > 1 and any(token in value for token in ['Enter to confirm', 'Claude Code v', 'Ask Codex']):
                            return value
                        time.sleep(0.15)
                    raise TimeoutError('CLI startup screen did not settle')

                text = settled_startup()
                if name == 'claude' and 'Yes, I trust this folder' in text:
                    # Trust only this freshly created empty test directory.
                    assert 'No, exit' in text and str(cwd) in text
                    assert lab.tmux_call('send-keys','-t',runtime['pane_id'],'Down').returncode == 0
                    wait_for(lambda: '❯ Yes, I trust this folder' in snapshot(runtime))
                    assert lab.tmux_call('send-keys','-t',runtime['pane_id'],'Enter').returncode == 0
                    text = settled_startup()
                assert ('Claude Code v' if name == 'claude' else marker) in text
                assert 'Yes, I trust this folder' not in text
                send_literal(runtime, DRAFT)
                wait_for(lambda: DRAFT in snapshot(runtime), timeout=15)
                started = run('ps','-p',runtime['pane_pid'],'-o','lstart=').stdout.strip()
                info = {'version':run(binary,'--version').stdout.strip(),'runtime_id':runtime['runtime_id'],'session_id':runtime['session_id'],'pane_pid':runtime['pane_pid'],'process_started':started,'sizes':[]}
                report['tools'][name] = info
                for stage, cols, rows in [('wide',120,32),('narrow',40,18),('restored',100,30)]:
                    target = '=' + runtime['tmux_name'] + ':0'
                    resized = lab.tmux_call('resize-window','-t',target,'-x',str(cols),'-y',str(rows))
                    assert resized.returncode == 0, resized.stderr
                    wait_for(lambda: inventory(runtime)['width'] == cols and inventory(runtime)['height'] == rows)
                    wait_for(lambda: 'P0_DRAFT' in snapshot(runtime))
                    info['sizes'].append(capture(name,stage,runtime,cols,rows,['P0_DRAFT','Привіт','漢字']))
                    current = stable(runtime, started)
                    assert (current['width'],current['height']) == (cols,rows)
                info['application_alternate_screen'] = lab.tmux_call('display-message','-p','-t',runtime['pane_id'],'#{alternate_on}').stdout.strip() == '1'
                check(name + ': Unicode draft rendered by xterm at 120x32, 40x18 and 100x30; process identity preserved')

            # Both actual CLIs stay alive while their host service is replaced.
            tmux_pid = lab.tmux_call('display-message','-p','#{pid}').stdout.strip()
            old_pid = lab.call('health')['agent_pid']
            run('launchctl','kickstart','-k',lab.domain+'/'+lab.prefix+'.agent')
            new_pid = wait_for(lambda: (pid if (pid := lab.call('health')['agent_pid']) != old_pid else None))
            report['agent_pids'] = [old_pid,new_pid]
            assert lab.tmux_call('display-message','-p','#{pid}').stdout.strip() == tmux_pid
            for name,runtime in runtimes.items():
                stable(runtime,report['tools'][name]['process_started'])
                report['tools'][name]['reattached'] = capture(name,'reattached',runtime,100,30,['P0_DRAFT','Привіт','漢字'])
                send_literal(runtime,' AFTER_RESTART')
                wait_for(lambda: 'AFTER_RESTART' in snapshot(runtime))
                stable(runtime,report['tools'][name]['process_started'])
                check(name + ': fresh attachment after agent restart restores draft; CLI accepts further unsubmitted input')
        except Exception:
            for name, runtime in runtimes.items():
                print(name + ' failure screen: ' + snapshot(runtime)[-4000:], file=sys.stderr)
            raise
        finally:
            for viewer in viewers:
                if viewer.fd is not None:
                    viewer.close(abrupt=True)
    (capture_dir/'index.json').write_text(json.dumps(captures,indent=2))
    report['cleanup'] = 'Temporary launchd jobs and actual CLI runtimes stopped; disposable workspaces removed.'
    report['limits'] = ['No model response/tool execution tested.','Resize/input are privileged local test-driver operations, not implemented product ownership APIs.','Raw captures contain local CLI account/status UI and remain in ignored .cache only.','No iPhone or remote/E2EE test.']
    return report


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--report',type=Path,required=True)
    parser.add_argument('--captures',type=Path,default=ROOT/'.cache/cli-captures')
    args = parser.parse_args()
    evidence = verify(args.captures)
    args.report.write_text(json.dumps(evidence,indent=2,ensure_ascii=False)+'\n')
    print(json.dumps(evidence,indent=2,ensure_ascii=False))
