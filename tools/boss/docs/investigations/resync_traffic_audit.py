"""Measure an isolated Boss app built by Bazel.

Usage: python3 /absolute/path/resync_traffic_audit.py LABEL /absolute/path/Boss.app/Contents/MacOS/Boss MODE
MODE is cold, reconnect, refresh, busy, or overflow. Outputs go to .codex/audit-LABEL.
An optional final argument sets the overflow inter-wave pause (default: 3 seconds).
Append `reset` after that pause for a ninth wave after 35 seconds of quiet.
The app bundle supplies the engine and CLI; all mutations target a fresh fixture socket.
Use a foreground timeout (900 seconds) around the invocation.
"""

import asyncio
import collections
import json
import os
from pathlib import Path
import sys
import time
import uuid

ROOT = Path(__file__).resolve().parents[4]
OUT = ROOT / '.codex' / ('audit-' + sys.argv[1])
OUT.mkdir(parents=True, exist_ok=True)
SOCKET = '/tmp/boss-audit-' + uuid.uuid4().hex[:12] + '.sock'
PROXY = SOCKET + '.proxy'
ENV = dict(os.environ, BOSS_SOCKET_PATH=SOCKET, BOSS_ENGINE_AUTOSTART='0',
           BOSS_ENGINE_DIAGNOSTICS_DIR=str(OUT / 'diagnostics'))
ENV.pop('BOSS_EVENTS_SOCKET', None)
APP = sys.argv[2]
CLI = str(Path(APP).parents[1] / "Resources/bin/boss")
ENGINE = str(Path(APP).parents[1] / "Resources/bin/engine")
MODE = sys.argv[3]
RESET_PROBE = len(sys.argv) > 5 and sys.argv[5] == 'reset'
if MODE not in {'cold', 'reconnect', 'refresh', 'busy', 'overflow'}:
    raise ValueError('unknown audit mode: ' + MODE)
traffic = []
connections = []
started = 0
throttle = False

async def cli(*args):
    proc = await asyncio.create_subprocess_exec(CLI, '--socket-path', SOCKET, '--json',
        '--no-input', '--no-autostart', '--no-engine-autostart', *args,
        env=ENV, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
    try:
        out, err = await asyncio.wait_for(proc.communicate(), 60)
    finally:
        if proc.returncode is None:
            proc.kill()
            await proc.wait()
    if proc.returncode:
        raise RuntimeError((args[:4], proc.returncode, err.decode()))
    return json.loads(out)

async def rpc(payload):
    reader, writer = await asyncio.open_unix_connection(SOCKET, limit=64*1024*1024)
    rid = uuid.uuid4().hex
    writer.write(json.dumps(dict(request_id=rid, payload=payload)).encode() + b'\n')
    await writer.drain()
    try:
        while True:
            line = await asyncio.wait_for(reader.readline(), 30)
            if not line:
                raise RuntimeError('engine closed metrics connection')
            event = json.loads(line)
            if event.get('request_id') == rid:
                return event['payload']
    finally:
        writer.close()
        await writer.wait_closed()

async def proxy(reader, writer):
    upstream, sink = await asyncio.open_unix_connection(SOCKET, limit=4096)
    connections.append(sink)
    async def forward(source, dest, direction):
        pending = b''
        while True:
            data = await source.read(4096 if direction == 'push' and throttle else 65536)
            if not data:
                break
            pending += data
            while b'\n' in pending:
                line, pending = pending.split(b'\n', 1)
                event = json.loads(line)
                payload = event.get('payload', event)
                traffic.append(dict(t=time.monotonic()-started, direction=direction,
                                    type=payload.get('type'), request_id=event.get('request_id'), bytes=len(line),
                                    payload={'event': payload.get('event', {}), 'topic': payload.get('topic'), 'limit': payload.get('limit'), 'flow': payload.get('flow')}))
            dest.write(data)
            await dest.drain()
            if direction == 'push' and throttle:
                await asyncio.sleep(.02)
    try:
        tasks = [asyncio.create_task(forward(reader, sink, 'request')),
                 asyncio.create_task(forward(upstream, writer, 'push'))]
        _, pending = await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
        for task in pending:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
    finally:
        sink.close()
        writer.close()

async def stop(proc):
    if proc is None or proc.returncode is not None:
        return
    proc.terminate()
    try:
        await asyncio.wait_for(proc.wait(), 10)
    except TimeoutError:
        proc.kill()
        await proc.wait()

async def main():
    global started, throttle
    log = (OUT / 'engine.log').open('w')
    engine = await asyncio.create_subprocess_exec(ENGINE, '--socket-path', SOCKET,
        env=ENV, stdout=log, stderr=log)
    app = None
    server = None
    try:
        for _ in range(600):
            if Path(SOCKET).exists():
                break
            if engine.returncode is not None:
                raise RuntimeError('engine exited; see engine.log')
            await asyncio.sleep(.1)
        products, projects = [], []
        for i in range(5):
            p = (await cli('product', 'create', '--name', f'Audit {i}'))['product']['id']
            products.append(p)
            for j in range(12):
                row = await cli('project', 'create', '--product', p, '--name', f'Project {j}', '--no-design-task')
                projects.append(row['project']['id'])
        (OUT / 'seed.json').write_text(json.dumps(dict(products=products, projects=projects)))
        before = await rpc(dict(type='metrics_list_live'))
        (OUT / 'metrics-before.json').write_text(json.dumps(before))
        server = await asyncio.start_unix_server(proxy, PROXY, limit=64*1024*1024)
        started = time.monotonic()
        applog = (OUT / 'app.log').open('w')
        app = await asyncio.create_subprocess_exec(APP, '--capture-to', str(OUT / 'capture.png'),
            '--capture-after', '180' if RESET_PROBE else ('125' if MODE == 'overflow' else '85'), '--capture-metrics',
            *(['--capture-refresh-work'] if MODE == 'refresh' else []),
            env=dict(ENV, BOSS_SOCKET_PATH=PROXY), stdout=applog, stderr=applog)
        await asyncio.sleep(10)
        if MODE == 'reconnect':
            for sink in list(connections):
                sink.close()
        elif MODE == 'busy':
            busy_start = time.monotonic()
            for i in range(20):
                await asyncio.sleep(max(0, busy_start + 3*i - time.monotonic()))
                await cli('project', 'update', projects[0], '--name', f'Busy {i}')
        elif MODE == 'overflow':
            throttle = True
            semaphore = asyncio.Semaphore(32)
            async def create(i):
                async with semaphore:
                    await cli('attention', 'create', '--kind', 'question', '--project', projects[0],
                              '--group-key', 'audit', '--question-type', 'prompt', '--prompt', f'Audit {i} ' + 'x'*8192)
            try:
                for wave in range(8):
                    throttle = True
                    await asyncio.gather(*(create(wave*600+i) for i in range(600)))
                    throttle = False
                    await asyncio.sleep(float(sys.argv[4]) if len(sys.argv) > 4 else 3)
                if RESET_PROBE:
                    await asyncio.sleep(35)
                    throttle = True
                    await asyncio.gather(*(create(4800+i) for i in range(600)))
                    throttle = False
            finally:
                throttle = False
        elif MODE == 'refresh':
            pass  # The capture invokes the real Refresh action at ten seconds.
        await asyncio.wait_for(app.wait(), 180)
        if app.returncode:
            raise RuntimeError(f'app exited {app.returncode}; see app.log')
        after = await rpc(dict(type='metrics_list_live'))
        (OUT / 'metrics-after.json').write_text(json.dumps(after))
    finally:
        (OUT / 'traffic.json').write_text(json.dumps(traffic))
        if server:
            server.close()
            await server.wait_closed()
        await stop(app)
        await stop(engine)
        log.close()
    requests = [x for x in traffic if x['direction'] == 'request']
    print(json.dumps(dict(requests=len(requests), counts=dict(collections.Counter(x['type'] for x in requests)),
                         output=str(OUT))), flush=True)

asyncio.run(main())
