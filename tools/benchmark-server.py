#!/usr/bin/env python3
"""Measure acknowledged edits across independent WebSockets and waves.

Requires aiohttp (`python3 -m pip install aiohttp`). Example:
  python3 tools/benchmark-server.py --binary target/release/gal-server

Uses a throwaway database populated with search entries outside the clients'
waves. Each socket waits for its own acknowledgement before submitting again;
many sockets exercise server contention instead of one socket's read loop.
Setup is excluded from timing. This is a local workload, not a capacity promise.
"""
import argparse
import asyncio
import json
import os
from pathlib import Path
import socket
import sqlite3
import statistics
import sys
import subprocess
import tempfile
import time

import aiohttp


async def receive(ws, kind):
    while True:
        message = await ws.receive_json(timeout=60)
        if message['type'] == 'error':
            raise RuntimeError(message)
        if message['type'] == kind:
            return message


def populate(path, creator, count):
    with sqlite3.connect(path) as conn:
        conn.execute("PRAGMA foreign_keys = ON")
        conn.execute("INSERT INTO waves(id,creator,created_at,mode) VALUES('w-load',?,0,'document')", (creator,))
        conn.execute("INSERT INTO wavelets(id,wave_id,kind,title,created_at,last_modified) VALUES('s-load','w-load','conversation','Ballast',0,0)")
        content = json.dumps({'ops': [{'insert': 'searchable ballast'}]})
        conn.executemany(
            "INSERT INTO blips(id,wavelet_id,wave_id,seq,author,contributors,created_at,last_modified,content,revision) VALUES(?,'s-load','w-load',?,?,?,?,0,?,0)",
            ((f'b-load-{i}', i, creator, '[]', 0, content) for i in range(count)),
        )
        conn.executemany(
            "INSERT INTO blip_search(blip_id,wave_id,body) VALUES(?,'w-load','searchable ballast')",
            ((f'b-load-{i}',) for i in range(count)),
        )
        if conn.execute("SELECT 1 FROM sqlite_master WHERE name='blip_search_keys'").fetchone():
            conn.execute("INSERT INTO blip_search_keys(id,blip_id) SELECT rowid,blip_id FROM blip_search WHERE wave_id='w-load'")


async def measure(args, base, database):
    async with aiohttp.ClientSession(cookie_jar=aiohttp.DummyCookieJar()) as http:
        cookies = []
        creator = None
        for i in range(4):
            async with http.post(base + '/api/register', json={
                'name': f'load{i}', 'password': 'correct horse battery',
            }) as response:
                response.raise_for_status()
                creator = (await response.json())['user']['id']
                cookies.append(response.headers['Set-Cookie'].split(';')[0])
        clients = []
        for i in range(args.sockets):
            ws = await http.ws_connect(base.replace('http:', 'ws:') + '/ws', headers={'Cookie': cookies[i % 4]})
            await receive(ws, 'welcome')
            await ws.send_json({'type': 'createWave', 'title': f'Load {i}', 'participants': []})
            state = await receive(ws, 'waveState')
            blip = state['wave']['wavelets'][0]['blips'][0]
            clients.append((ws, blip['id'], blip['revision']))
        await asyncio.to_thread(populate, database, creator, args.entries)
        latencies = []

        async def edit(ws, blip, revision, client):
            for i in range(args.edits):
                ops = ([{'retain': i}] if i else []) + [{'insert': 'x'}]
                started = time.perf_counter()
                await ws.send_json({'type': 'submit', 'blipId': blip, 'revision': revision,
                                    'delta': {'ops': ops}, 'opId': f'load-{client}-{i}'})
                ack = await receive(ws, 'ack')
                assert ack['blipId'] == blip and ack['revision'] == revision + 1, ack
                revision = ack['revision']
                latencies.append((time.perf_counter() - started) * 1000)

        started = time.perf_counter()
        await asyncio.gather(*(edit(*client, i) for i, client in enumerate(clients)))
        elapsed = time.perf_counter() - started
        with sqlite3.connect(database) as conn:
            for _, blip, _ in clients:
                content, revision = conn.execute('SELECT content,revision FROM blips WHERE id=?', (blip,)).fetchone()
                assert revision == args.edits, (blip, revision)
                assert json.loads(content) == {'ops': [{'insert': 'x' * args.edits}]}, content
        for ws, _, _ in clients:
            await ws.close()
        ordered = sorted(latencies)
        return {'binary': str(args.binary), 'search_entries': args.entries,
                'sockets': args.sockets, 'acknowledged_edits': len(latencies),
                'seconds': round(elapsed, 3), 'edits_per_second': round(len(latencies) / elapsed, 1),
                'ack_median_ms': round(statistics.median(latencies), 3),
                'ack_p95_ms': round(ordered[int((len(ordered) - 1) * .95)], 3),
                'persisted_documents_verified': len(clients)}


async def main(args):
    with tempfile.TemporaryDirectory(prefix='gal-load-') as directory:
        database = Path(directory) / 'load.db'
        with socket.socket() as probe:
            probe.bind(('127.0.0.1', 0))
            port = probe.getsockname()[1]
        env = {k: v for k, v in os.environ.items() if not k.startswith('GAL_')}
        env.update(GAL_HOST='127.0.0.1', GAL_PORT=str(port), GAL_DB=str(database))
        with open(Path(directory) / 'server.log', 'w+') as log:
            process = subprocess.Popen([str(args.binary.resolve())], env=env, stdout=log, stderr=log)
            try:
                base = f'http://127.0.0.1:{port}'
                async with aiohttp.ClientSession() as http:
                    for _ in range(100):
                        if process.poll() is not None:
                            log.seek(0)
                            raise RuntimeError(log.read())
                        try:
                            async with http.get(base + '/healthz') as response:
                                if response.status == 200:
                                    break
                        except aiohttp.ClientError:
                            pass
                        await asyncio.sleep(.1)
                    else:
                        raise RuntimeError('server did not become healthy')
                print(json.dumps(await measure(args, base, database), indent=2))
            except Exception:
                log.flush()
                log.seek(0)
                print(log.read()[-6000:], file=sys.stderr)
                raise
            finally:
                process.terminate()
                try:
                    await asyncio.to_thread(process.wait, timeout=15)
                except subprocess.TimeoutExpired:
                    process.kill()
                    await asyncio.to_thread(process.wait)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=Path('target/release/gal-server'))
    parser.add_argument('--entries', type=int, default=100_000)
    parser.add_argument('--sockets', type=int, default=16)
    parser.add_argument('--edits', type=int, default=80)
    options = parser.parse_args()
    if not 1 <= options.sockets <= 96 or options.entries < 0 or options.edits < 1:
        parser.error('use 1..96 sockets, nonnegative entries, and positive edits')
    asyncio.run(main(options))
