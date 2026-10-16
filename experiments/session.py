#!/usr/bin/env python3
"""Silent sustained-session proof: queue handoffs, seeks, retries and retirement.

One client/server/Navidrome lifetime. No audio-device option. Scripted qualities
and a Sidecar-to-client outage isolate lifecycle and recovery behavior.
"""
import argparse
import http.client
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time

import handoff
import live
import navidrome as nav
from mixed_handoff import convert_reference, make_plans
from mixed_seek import MixedClient, TransientStreamError
from output_clock import Converter, mapping
from seek import SeekServer, Ticket
from seek_output import FileTimeline


TRANSIENT = (TransientStreamError, ConnectionError, socket.timeout, http.client.IncompleteRead,
             http.client.RemoteDisconnected)


def retry_fetch(fetch, ticket, record, timeout=20):
    """Only incomplete transport may retry; integrity/auth/format errors fail."""
    deadline, attempt = time.monotonic() + timeout, 0
    while True:
        ticket.check()
        nav.require(time.monotonic() < deadline, 'stream retry budget exhausted')
        try:
            return fetch()
        except TRANSIENT as error:
            ticket.check()
            remaining = deadline - time.monotonic()
            nav.require(remaining > 0, 'stream retry budget exhausted')
            delay = min(1, 0.25 * 2 ** min(attempt, 2), remaining)
            record({'attempt': attempt + 1, 'reason': type(error).__name__, 'delay': delay,
                    'at': time.monotonic()})
            attempt += 1
            ticket.cancelled.wait(delay)


class SessionServer(SeekServer):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.outage_until = 0
        self.outages, self.refusals, self.retired = [], [], []

    def session_key(self, generation, track):
        return generation, track

    def rate(self):
        return 1250000

    def outage(self, seconds):
        now = time.monotonic()
        self.outage_until = now + seconds
        self.outages.append({'began': now, 'ended': self.outage_until})

    def before_request(self, handler):
        if time.monotonic() >= self.outage_until:
            return True
        self.refusals.append(time.monotonic())
        handler.send_response(503)
        handler.send_header('Content-Length', '0')
        handler.send_header('Connection', 'close')
        handler.end_headers()
        return False

    def write(self, handler, data, cancel):
        if time.monotonic() < self.outage_until:
            handler.proof_record['outage_interrupted'] = True
            raise ConnectionResetError('scripted segment-hop outage')
        super().write(handler, data, cancel)

    def retire(self, generation):
        self.cancel(generation)
        deadline = time.monotonic() + 10
        while True:
            with self.lock:
                active = any(r['generation'] == generation and 'finished' not in r for r in self.records)
                if not active:
                    owned = [(k, self.sessions.pop(k)) for k in list(self.sessions) if k[0] == generation]
                    break
            nav.require(time.monotonic() < deadline, 'retired server request remained')
            time.sleep(.02)
        for key, session in owned:
            session.close()
            self.retired.append({'generation': key[0], 'track': key[1], **session.summary()})
            nav.require(not session.source or not session.source.blocks, 'retired source bytes remained')


class SessionClient(MixedClient):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.output_holds, self.input_holds = {}, {}
        self.by_generation, self.retries = {}, []
        self.monitor = None

    def switch_queue(self, queue, origins, reason):
        selected = [self.infos[i] for i in queue]
        parts = mapping(selected, [{'origin': o, 'frames': info['frames'] - o}
                                   for info, o in zip(selected, origins)], self.output_rate)
        with self.lock:
            old = self.generation
            self.generation += 1
            gen = self.generation
            if old >= 0:
                if old in self.tickets:
                    self.tickets[old].cancel()
                self.sink.switch(gen, sum(p['output_frames'] for p in parts))
            self.plans[gen] = {'generation': gen, 'queue': queue, 'parts': parts,
                'track': queue[0], 'sample_rate': selected[0]['sample_rate'], 'segments': [],
                'requested': time.monotonic(), 'reason': reason}
            self.tickets[gen] = Ticket()
        if old >= 0:
            self.server.cancel(old)
        thread = threading.Thread(target=self.produce, args=(gen,), daemon=True)
        self.by_generation[gen] = thread
        self.workers.append(thread)
        thread.start()
        print(f'generation {gen}: {reason}', flush=True)
        return gen

    def send_part(self, gen, index, handle, start, pcm):
        hold = self.output_holds.get(gen)
        if hold and not hold[0].is_set():
            hold[0].set()
            nav.require(hold[1].wait(15), 'old output barrier timed out')
        with self.lock:
            if gen != self.generation:
                self.stale_rejected += 1
                raise nav.Cancelled('late converted output')
            nav.require(self.sink.append(gen, start, pcm), 'current PCM refused')
            handle.write(pcm)

    def produce(self, gen):
        plan, ticket = self.plans[gen], self.tickets[gen]
        try:
            for index, part in enumerate(plan['parts']):
                ticket.check()
                plan.update(track=plan['queue'][index], sample_rate=part['sample_rate'])
                folder = self.output / f'generation-{gen}/part-{index}'
                folder.mkdir(exist_ok=True)
                with (folder / 'output.s32le').open('wb') as handle:
                    converter = Converter(part, self.output_rate,
                        lambda start, pcm: self.send_part(gen, index, handle, start, pcm), folder,
                        cancel=ticket.cancelled)
                    with self.lock:
                        self.converters[(gen, index)] = converter
                    try:
                        for number, segment in enumerate(make_plans([self.infos[plan['track']]], [part])):
                            while self.sink.snapshot(gen)['buffered'] >= 8:
                                ticket.check()
                                ticket.cancelled.wait(.02)
                            ticket.check()
                            quality = ('lossless', 'opus-64', 'opus-128')[(number + index) % 3]
                            def fetch():
                                return self.fetch_native(gen, segment, quality, folder=folder)
                            pcm, record, observation = retry_fetch(fetch, ticket,
                                lambda event: self.retries.append({'generation': gen, 'part': index,
                                    'source_start': segment['source_start'], **event}))
                            hold = self.input_holds.get(gen)
                            if hold and not hold[0].is_set():
                                hold[0].set()
                                nav.require(hold[1].wait(15), 'old input barrier timed out')
                            record.update(track=index, pcm=f'part-{index}/' + record['pcm'])
                            if not self.admit_native(gen, record, observation):
                                raise nav.Cancelled('late native completion')
                            converter.append(segment['source_start'], pcm)
                        self.conversions[(gen, index)] = converter.finish()
                    finally:
                        converter.close()
                        self.conversions.setdefault((gen, index), {'cancelled': ticket.cancelled.is_set()})
                        self.conversions[(gen, index)].update(closed=converter.closed,
                            process_stopped=converter.process is None or converter.process.poll() is not None)
        except BaseException as error:
            if not ticket.cancelled.is_set():
                self.error = error

    def wait(self, predicate, message, timeout=60):
        deadline = time.monotonic() + timeout
        while not predicate():
            if self.error:
                raise self.error
            if self.monitor and self.monitor.error:
                raise self.monitor.error
            nav.require(time.monotonic() < deadline, message)
            snap = self.sink.snapshot(self.generation)
            if self.generation in self.sink.players:
                player = self.sink.players[self.generation]
                if player.error and not player.cancel.is_set():
                    raise player.error
            time.sleep(.01)

    def retire(self, gen):
        worker = self.by_generation.pop(gen)
        worker.join(10)
        nav.require(not worker.is_alive(), 'retired producer remained')
        self.workers.remove(worker)
        self.server.retire(gen)
        self.sink.retire(gen)
        for key in [k for k in self.converters if k[0] == gen]:
            converter = self.converters.pop(key)
            nav.require(converter.closed and (converter.process is None or converter.process.poll() is not None),
                        'retired converter remained')
        self.tickets.pop(gen)
        self.output_holds.pop(gen, None)
        self.input_holds.pop(gen, None)

    def stop(self):
        for holds in (self.output_holds, self.input_holds):
            for held, release in holds.values():
                release.set()
        super().stop()


class Monitor:
    """Measured running resources. Captures/checker memory are outside this run."""
    def __init__(self, client):
        self.client = client
        self.samples, self.checkpoints = [], []
        self.done = threading.Event()
        self.error = None
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def sample(self):
        c = self.client
        with c.server.lock:
            sessions = list(c.server.sessions.values())
            active = sum('finished' not in r for r in c.server.records)
        # Individual caches are bounded by RangeSource. Count actual retained
        # bytes without waiting on the whole session/acquisition lock.
        cached = 0
        for session in sessions:
            if session.source:
                with session.source.lock:
                    cached += sum(map(len, session.source.blocks.values()))
        with c.lock:
            players = list(c.sink.players.values())
            converters = list(c.converters.values())
        queued = sum(p.buffered() for p in players)
        rss = int(subprocess.check_output(['ps', '-o', 'rss=', '-p', str(os.getpid())]).strip())
        children = subprocess.check_output(['ps', '-axo', 'pid=,ppid=,comm=']).decode().splitlines()
        tree = [(int(v[0]), int(v[1]), Path(v[2]).name) for line in children if len(v := line.split(None, 2)) == 3]
        owned = {os.getpid()}
        while True:
            added = {pid for pid, parent, _ in tree if parent in owned} - owned
            if not added:
                break
            owned.update(added)
        processes = [name for pid, _, name in tree if pid in owned and pid != os.getpid() and name != 'ps']
        return {'at': time.monotonic(), 'rss_kib': rss, 'fds': len(os.listdir('/dev/fd')),
            'threads': threading.active_count(), 'source_sessions': len(sessions), 'cached_bytes': cached,
            'active_requests': active, 'consumers': len(players), 'queued_seconds': queued,
            'live_converters': sum(x.process is not None and x.process.poll() is None for x in converters),
            'child_processes': processes}

    def run(self):
        try:
            while not self.done.is_set():
                value = self.sample()
                self.samples.append(value)
                nav.require(value['source_sessions'] <= 6 and value['cached_bytes'] <= 6 * 8 * 1024 * 1024,
                            'source cache/session bound exceeded')
                nav.require(value['live_converters'] <= 3 and value['queued_seconds'] < 25,
                            'old playback state accumulated')
                nav.require(value['rss_kib'] < 384 * 1024 and value['fds'] < 96 and value['threads'] < 32,
                            'process resource ceiling exceeded')
                self.done.wait(.5)
        except BaseException as error:
            self.error = error

    def checkpoint(self):
        value = self.sample()
        nav.require(value['source_sessions'] == value['cached_bytes'] == value['live_converters'] == value['consumers'] == 0,
                    'retirement retained playback resources')
        nav.require(not any(n in ('ffmpeg', 'stratum-poc') for n in value['child_processes']), 'decoder child remained')
        self.checkpoints.append(value)

    def stop(self):
        self.done.set()
        self.thread.join(6)
        nav.require(not self.thread.is_alive(), 'resource monitor remained')
        if self.error:
            raise self.error


def verify(folder, plan, snapshot, sources, output_rate, checker):
    tracks = []
    output_bytes = 0
    for index, part in enumerate(plan['parts']):
        directory = folder / f'part-{index}'
        directory.mkdir(exist_ok=True)
        path = directory / 'output.s32le'
        if not path.exists():
            path.touch()
        actual = path.read_bytes()
        segments = [s for s in plan['segments'] if s['track'] == index]
        if actual:
            native = directory / 'reference.s32le'
            with native.open('wb') as stream:
                for segment in segments:
                    stream.write((folder / segment['pcm']).read_bytes())
            expected = convert_reference(native, part['sample_rate'], output_rate,
                part['discard_output_start'], len(actual) // 8, raw=True)
            nav.require(actual == expected, 'continuous conversion prefix mismatch')
        output_bytes += len(actual)
        tracks.append({**part, 'source': str(sources[plan['queue'][index]]), 'output_pcm': f'part-{index}/output.s32le'})
    nav.require(output_bytes == snapshot['accepted'] * 8, 'accepted output count mismatch')
    delivery = {'seek': True, 'output_rate': output_rate, 'frames': snapshot['accepted'],
        'rendered_media_frames': snapshot['consumed'], 'tracks': tracks,
        'segments': plan['segments'], 'underruns': snapshot['underruns']}
    path = folder / 'delivery.json'
    path.write_text(json.dumps(delivery, indent=2) + '\n')
    checked = json.loads(nav.run([str(checker), str(path)]))
    nav.require(checked['exact_frames'] and checked['rendered_matches'], 'captured timeline mismatch')
    for i, track in enumerate(checked['tracks']):
        if any(s['track'] == i for s in plan['segments']):
            nav.require(track['lossless_exact'] and handoff.alignment_passes(track), 'native source mismatch')
    return checked


def run_round(client, index):
    gen = client.switch_queue([0, 1, 2, 0], [0, 0, 0, 0], f'round {index + 1}: four-track queue')
    if index == 0:
        client.wait(lambda: client.sink.snapshot(gen)['consumed'] >= 8 * client.output_rate
            and any(j['generation'] == gen and j['stage'] == 'download' and 'finished' not in j
                    and j['received_bytes'] >= 8192 for j in client.jobs), 'no download for outage')
        client.server.outage(12)
        print('12-second segment-hop outage started', flush=True)
        client.wait(lambda: client.sink.snapshot(gen)['silence_frames'] > 0, 'outage never exhausted the buffer')
        at = client.sink.snapshot(gen)['consumed']
        client.wait(lambda: time.monotonic() >= client.server.outage_until
                    and client.sink.snapshot(gen)['consumed'] > at + client.output_rate, 'playback did not recover')
        print('outage recovered at the same media position', flush=True)
    boundary = client.plans[gen]['parts'][2]['output_start']
    client.wait(lambda: client.sink.snapshot(gen)['consumed'] >= boundary + client.output_rate,
                'two natural handoffs did not play')
    output_hold = threading.Event(), threading.Event()
    client.output_holds[gen] = output_hold
    client.wait(output_hold[0].is_set, 'no converted output to race')
    # Install the next barrier before starting its worker.
    mid = gen + 1
    input_hold = threading.Event(), threading.Event()
    client.input_holds[mid] = input_hold
    rate = client.infos[0]['sample_rate']
    client.switch_queue([0, 1], [3 * rate + 37, 0], 'seek backward during pending output')
    client.wait(input_hold[0].is_set, 'no native completion to race')
    final = client.switch_queue([2], [197], 'rapid skip supersedes the seek')
    output_hold[1].set()
    input_hold[1].set()
    client.wait(lambda: client.stale_rejected == index + 1 and client.input_rejected == index + 1,
                'old audio passed an admission boundary')
    client.retire(gen)
    client.retire(mid)
    nav.require(client.sink.snapshot(mid)['accepted'] == client.sink.snapshot(mid)['consumed'] == 0,
                'superseded seek produced audio')
    client.wait(lambda: client.sink.snapshot(final)['done'], 'final skip did not finish')
    nav.require(client.sink.snapshot(final)['consumed'] == client.plans[final]['parts'][0]['output_frames'],
                'final skip lost media')
    client.retire(final)
    client.monitor.checkpoint()
    print(f'round {index + 1} complete; all queues, converters and source sessions retired', flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('navidrome', 'stratum', 'checker', 'out'):
        parser.add_argument('--' + name, type=Path, required=True)
    parser.add_argument('--source', type=Path, action='append', required=True)
    parser.add_argument('--rounds', type=int, choices=range(1, 9), default=4)
    args = parser.parse_args()
    sources = [p.resolve(strict=True) for p in args.source]
    nav.require(len(sources) == 3, 'provide three short synthetic sources')
    infos = [handoff.inspect_source(p, args.stratum) for p in sources]
    nav.require([i['sample_rate'] for i in infos] == [44100, 48000, 96000], 'expected three native clocks in order')
    nav.require(all(12 <= i['frames'] / i['sample_rate'] < 13 for i in infos), 'use 12-second fixtures plus an odd tail')
    args.out.mkdir(parents=True, exist_ok=False)
    output = args.out.resolve()
    first = mapping([infos[i] for i in [0, 1, 2, 0]],
                    [{'origin': 0, 'frames': infos[i]['frames']} for i in [0, 1, 2, 0]], 48000)
    sink = FileTimeline(output, sum(p['output_frames'] for p in first), rate=48000)
    client, monitor = None, None
    began = time.monotonic()
    try:
        with tempfile.TemporaryDirectory(prefix='stratum-session-nav-') as temporary:
            with nav.disposable_library(args.navidrome, sources, Path(temporary), lifetime=60 + 65 * args.rounds) as (account, songs, _):
                with SessionServer(account, songs, infos, args.stratum, output, sink,
                                   max_generations=3 * args.rounds, max_requests=100 * args.rounds).serve() as server:
                    client = SessionClient(server, sink, output, infos, 48000)
                    monitor = Monitor(client)
                    client.monitor = monitor
                    for index in range(args.rounds):
                        run_round(client, index)
    finally:
        if client:
            client.stop()
        else:
            sink.stop()
        if monitor:
            monitor.stop()
    elapsed = time.monotonic() - began
    checks = [verify(output / f'generation-{gen}', plan, sink.snapshot(gen), sources, 48000, args.checker)
              for gen, plan in client.plans.items()]
    nav.require(all(g == 0 or snapshot['silence_frames'] == 0 for g, snapshot in sink.archived.items()),
                'unexpected starvation outside outage generation')
    nav.require(any(r.get('outage_interrupted') for r in server.records) and server.refusals,
                'outage did not interrupt transport and refuse retries')
    nav.require(not any('error' in r for r in server.records), 'server request failed unexpectedly')
    nav.require(all(r['closed'] for r in server.retired) and not server.sessions, 'source session remained')
    quiet = monitor.checkpoints
    nav.require(max(s['fds'] for s in quiet) - min(s['fds'] for s in quiet) <= 2, 'idle descriptor count grew')
    nav.require(max(s['rss_kib'] for s in quiet) - min(s['rss_kib'] for s in quiet) < 48 * 1024, 'idle memory drift exceeded bound')
    report = {'experiment': 'stratum-session-poc-1', 'rounds': args.rounds, 'elapsed_seconds': elapsed,
        'output_rate': 48000, 'source_info': infos, 'generations': [{**p, **sink.snapshot(g)} for g, p in client.plans.items()],
        'audio_checks': checks, 'stale_native_rejected': client.input_rejected, 'stale_converted_rejected': client.stale_rejected,
        'outages': server.outages, 'refused_requests': len(server.refusals), 'retries': client.retries,
        'server_requests': server.records, 'retired_source_sessions': server.retired,
        'resources': {'samples': monitor.samples, 'quiescent': quiet},
        'conversions': {str(k): v for k, v in client.conversions.items()}, 'server_stopped': True,
        'scope': 'Silent wall-clock proof, scripted quality changes, segment-hop outage; no Astra/device integration.'}
    (output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
    print(f'Sustained session verified ({elapsed:.1f}s): {output / "report.json"}', flush=True)


if __name__ == '__main__':
    main()
