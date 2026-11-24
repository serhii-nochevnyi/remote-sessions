#!/usr/bin/env python3
"""Real host ownership + input; opt-in macOS launchd integration, no model calls."""
import json
from pathlib import Path
import shutil
import socket
import sys
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'scripts'))
from runtime_lab import Lab, ROOT, run, wait_for

with Lab() as lab:
    probe = lab.state / 'probe.py'
    shutil.copy2(ROOT / 'tests/integration/runtime_probe.py', probe)
    received = lab.state / 'input.txt'
    runtime = lab.call('create', '--cwd', lab.state, '--', sys.executable, probe, lab.state/'heartbeat.json', received)['runtime']
    def call(op, **fields):
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(6)
            client.connect(str(lab.state/'agent.sock'))
            client.sendall((json.dumps({'op':op,'runtime_id':runtime['runtime_id'],**fields})+'\n').encode())
            return json.loads(client.makefile('rb').readline())
    first = call('acquire')['token']
    assert call('acquire')['result'] == 'error'
    payload = 'Привіт desktop \x1b[A\x1b[B\t\r'.encode()
    assert call('input',token=first,sequence=0,data=list(payload))['result'] == 'accepted'
    wait_for(lambda: received.exists() and 'Привіт desktop' in received.read_text())
    assert received.read_bytes() == payload.replace(b'\r',b'\n')
    assert call('input',token=first,sequence=0,data=[13])['result'] == 'error'
    assert call('input',token=first,sequence=1,data=[65]*4097)['result'] == 'error'
    assert call('input',token=first,sequence=1,data=list('Вставка з Mac'.encode()),paste=True)['result'] == 'paste_unavailable'
    assert received.read_bytes() == payload.replace(b'\r',b'\n')
    ordinary = 'Ручний ввід\r'.encode()
    assert call('input',token=first,sequence=1,data=list(ordinary),paste=False)['result'] == 'accepted'
    wait_for(lambda: 'Ручний ввід\n' in received.read_text())
    expected = (payload + ordinary).replace(b'\r',b'\n')
    assert received.read_bytes() == expected
    old_pid = lab.call('health')['agent_pid']
    run('launchctl','kickstart','-k',lab.loaded[1])
    wait_for(lambda: lab.call('health')['agent_pid'] != old_pid)
    assert call('input',token=first,sequence=1,data=[13])['result'] == 'error'
    second = call('acquire')['token']
    assert second != first
    call('release',token=first)
    assert call('acquire')['result'] == 'error'
    assert call('input',token=second,sequence=0,data=list(b'after restart\r'))['result'] == 'accepted'
    wait_for(lambda: 'after restart' in received.read_text())
    call('release',token=second)
    current = lab.call('list')['runtimes'][0]
    assert current['pane_pid'] == runtime['pane_pid']
    # Actual same bridge used by Tauri; acquire/read/input/detach/reconnect.
    result = run(ROOT/'target/debug/examples/probe',lab.state,runtime['runtime_id'])
    assert 'PASS' in result.stdout
    wait_for(lambda: received.read_text().count('Native bridge — Україна') == 3)
    assert received.read_bytes() == expected + b'after restart\n' + 'Native bridge — Україна\n'.encode() * 3
    print('PASS Unicode, arrow bytes, Enter, exact input, exclusive writer, duplicate rejection, stale token rejection after restart, same runtime PID, native bridge 3 cycles')
