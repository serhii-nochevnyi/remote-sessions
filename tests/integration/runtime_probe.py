#!/usr/bin/env python3
"""Deterministic real PTY process; no model calls, user code or credentials."""
import json
import os
from pathlib import Path
import sys
import threading
import time
import uuid

heartbeat = Path(sys.argv[1])
received = Path(sys.argv[2])
instance = uuid.uuid4().hex


def read_input():
    for line in sys.stdin:
        with received.open('a') as stream:
            stream.write(line)


threading.Thread(target=read_input, daemon=True).start()
for sequence in range(100_000):
    state = {'pid': os.getpid(), 'instance': instance, 'sequence': sequence}
    pending = heartbeat.with_suffix('.tmp')
    pending.write_text(json.dumps(state))
    pending.replace(heartbeat)
    print(f'P0 runtime alive · Робота триває · tick={sequence}', flush=True)
    time.sleep(0.2)
