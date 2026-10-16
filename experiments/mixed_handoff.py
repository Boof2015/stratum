#!/usr/bin/env python3
"""Mixed native-rate Navidrome tracks on one explicitly selected output clock.

Normal processed playback proof only. Source delivery remains native; output
conversion is in the client. No device is opened unless explicitly selected.
"""
import argparse
import json
from pathlib import Path
import tempfile
import threading
import time

import handoff
import live
import navidrome as nav
from output_clock import Converter, mapping, resample_filter


def make_plans(infos, parts):
    plans = []
    for track, (info, part) in enumerate(zip(infos, parts)):
        step = 2 * info['sample_rate'] + 1
        spans = [(part['input_origin'], part['origin']),
                 (part['origin'], part['origin'] + part['frames']),
                 (part['origin'] + part['frames'], part['input_end'])]
        for begin, end in spans:
            for start in range(begin, end, step):
                plans.append({'track': track, 'source_start': start, 'start': start,
                              'frames': min(step, end - start)})
    return plans


class NativeInput:
    def __init__(self, converter, playback):
        self.converter, self.playback = converter, playback

    @property
    def started(self):
        return self.playback.started

    def buffered(self):
        return self.playback.buffered()

    def append(self, source_start, pcm):
        self.converter.append(source_start, pcm)


def convert_reference(path, input_rate, output_rate, skip, frames, raw=False):
    command = nav.FFMPEG + (['-f', 's32le', '-ar', str(input_rate), '-ac', '2'] if raw else [])
    command += ['-i', str(path), '-map', '0:a:0', '-af',
                resample_filter(output_rate) + f',atrim=start_sample={skip}:end_sample={skip + frames}',
                '-c:a', 'pcm_s32le', '-f', 's32le', 'pipe:1']
    result = nav.run(command)
    nav.require(len(result) == frames * 8, 'independent output reference is short')
    return result


def verify_output(output, delivery):
    """One-shot references are independent of the streaming pipe/packet boundaries."""
    checks = []
    for index, part in enumerate(delivery['tracks']):
        segments = [s for s in delivery['segments'] if s['track'] == index]
        native = output / f'track-{index}/reference-input.s32le'
        with native.open('wb') as sink:
            for segment in segments:
                sink.write((output / segment['pcm']).read_bytes())
        reference = convert_reference(native, part['sample_rate'], delivery['output_rate'],
                                      part['discard_output_start'], part['output_frames'], raw=True)
        actual = (output / part['output_pcm']).read_bytes()
        nav.require(actual == reference, 'streaming conversion differs from continuous reference')
        lossless = all(s['quality'] == 'lossless' for s in segments)
        if lossless:
            original = convert_reference(part['source'], part['sample_rate'], delivery['output_rate'],
                                         part['origin'] * delivery['output_rate'] // part['sample_rate'],
                                         part['output_frames'])
            nav.require(actual == original, 'output conversion lost original source phase/samples')
        checks.append({'streaming_matches_continuous': True, 'lossless_matches_full_source': True if lossless else None})
    return checks


def run_case(name, account, songs, infos, sources, args, output):
    output.mkdir()
    # First track's actual EOF, then the second track's actual beginning. The
    # one-frame first-track tail is deliberately retained in the native plans.
    ranges = [{'origin': infos[0]['frames'] - (12 * infos[0]['sample_rate'] + 7),
               'frames': 12 * infos[0]['sample_rate'] + 7},
              {'origin': 0, 'frames': 20 * infos[1]['sample_rate'] + 9}]
    parts = mapping(infos, ranges, args.output_rate)
    plans = make_plans(infos, parts)
    total = sum(part['output_frames'] for part in parts)
    boundary = parts[0]['output_frames']
    if args.audio_helper:
        from device_output import DevicePlayback
        playback = DevicePlayback(output, args.audio_helper, args.audio_device, total, args.audio_gain, rate=args.output_rate)
    else:
        playback = live.Playback(output, total=total, start_buffer=6 * args.output_rate, rate=args.output_rate)
    converters, inputs, handles = [], [], []
    segments, retries, conversions = [], [], []
    policy = live.Policy(infos[0]['bytes'] / (infos[0]['frames'] / infos[0]['sample_rate']) * 2)
    began = time.monotonic()
    player = threading.Thread(target=playback.play, daemon=True)
    try:
        for index, part in enumerate(parts):
            directory = output / f'track-{index}'
            directory.mkdir()
            handle = (directory / 'output.s32le').open('wb')
            handles.append(handle)
            def send(start, pcm, sink=handle):
                playback.append(start, pcm)
                sink.write(pcm)
            converter = Converter(part, args.output_rate, send, directory)
            converters.append(converter)
            inputs.append(NativeInput(converter, playback))
        player.start()
        with handoff.endpoint(account, songs, infos, args.stratum, output, playback, plans,
                              name in ('mixed', 'adaptive', 'late-next')) as (url, records, sessions):
            for index, plan in enumerate(plans):
                handoff.wait_until(playback, lambda: playback.buffered() < 8)
                track = plan['track']
                rate = infos[track]['sample_rate']
                if track == 1 and plan['source_start'] == 0:
                    delay = 5.25 if name in ('mixed', 'adaptive') else boundary / args.output_rate + 0.5
                    if name in ('mixed', 'adaptive', 'late-next'):
                        handoff.wait_until(playback, lambda: playback.started is not None and time.monotonic() - playback.started >= delay)
                if name == 'adaptive':
                    policy.lossless_bytes = infos[track]['bytes'] / (infos[track]['frames'] / rate) * 2
                    quality = policy.choose(playback.buffered()) if playback.started is not None else 'lossless'
                elif name == 'reverse':
                    quality = 'opus-64' if track == 0 else 'lossless'
                else:
                    quality = 'opus-64' if track == 1 and name in ('mixed', 'late-next') else 'lossless'
                try:
                    record = handoff.receive(url, index, plan, quality, policy, inputs[track], name == 'adaptive', output, rate)
                except live.RetryLower:
                    retries.append({**plan, 'from': quality, 'at': time.monotonic()})
                    record = handoff.receive(url, index, plan, 'opus-64', policy, inputs[track], True, output, rate)
                segments.append(record)
                if index == 0 or plans[index - 1]['track'] != track or segments[-2]['quality'] != record['quality']:
                    print(f'{name}: track {track + 1}, {rate} Hz → {args.output_rate} Hz output, {record["quality"]}', flush=True)
                if plan['source_start'] + plan['frames'] == parts[track]['input_end']:
                    conversions.append(converters[track].finish())
                    handles[track].flush()
            nav.require(playback.done.wait(60), 'consumer did not finish')
            if playback.error:
                raise playback.error
    finally:
        playback.cancel.set()
        playback.ready.set()
        if args.audio_helper:
            playback.stop()
        if player.ident is not None:
            player.join(timeout=5)
        for converter in converters:
            converter.close()
        for handle in handles:
            handle.close()
        nav.require(not player.is_alive(), 'consumer remained')
    nav.require(not any('error' in record for record in records), 'segment request failed')
    ready = converters[1].first_output_at
    nav.require(ready is not None, 'no next-track output')
    margin = playback.started + boundary / args.output_rate - ready
    if name == 'late-next':
        nav.require(playback.silence_frames > 0 and margin < 0, 'negative control missed no deadline')
    else:
        nav.require(playback.silence_frames == 0 and margin > 0, 'mixed-rate handoff starved')
    if name in ('mixed', 'adaptive'):
        first_next = next(s for s in segments if s['track'] == 1)
        nav.require(5 <= first_next['began'] - playback.started < 17, 'next source prebuilt before slow link')
    if args.audio_helper:
        nav.require(playback.device_report['timeline_discontinuities'] == 0, 'output queue reset at handoff')
        live.validate_device_timing(playback.device_report, (total + playback.silence_frames) / args.output_rate)
    else:
        nav.require(abs(playback.elapsed - (total + playback.silence_frames) / args.output_rate) < 0.1,
                    'consumer output clock mismatch')
    for track, session in enumerate(sessions):
        metrics = session.summary()
        requests = [r for r in records if r['track'] == track]
        nav.require(metrics['validation_requests'] == len(requests), 'cached request skipped authorization')
        nav.require(metrics['upstream_body_bytes'] == sum(r['acquisition']['body_bytes_received'] for r in requests),
                    'incorrect source byte accounting')
        nav.require(metrics['peak_cached_bytes'] <= metrics['cache_limit_bytes'], 'source cache exceeded')
    delivery = {'output_rate': args.output_rate, 'frames': total,
                'tracks': [{**part, 'source': str(source), 'output_pcm': f'track-{i}/output.s32le'}
                           for i, (part, source) in enumerate(zip(parts, sources))],
                'segments': segments, 'underruns': playback.underruns}
    (output / 'delivery.json').write_text(json.dumps(delivery, indent=2) + '\n')
    native = json.loads(nav.run([str(args.checker), str(output / 'delivery.json')]))
    nav.require(native['exact_frames'] and native['rendered_matches'], 'mixed output accounting/capture mismatch')
    for index, track in enumerate(native['tracks']):
        nav.require(track['lossless_exact'] and handoff.alignment_passes(track), 'native segment verification failed')
        if not track['zero_lag']:
            print(f"{name}: track {index + 1} has {len(track['nonzero_alignment_windows'])} sub-codec-frame native estimates; "
                  'independent Opus-clock alignment passed (see report)', flush=True)
    converted = verify_output(output, delivery)
    report = {'scenario': name, 'source_rates': [i['sample_rate'] for i in infos], 'output_rate': args.output_rate,
              'policy': 'fixed-client-output; native source delivery; cumulative rational duration',
              'frames': total, 'boundary_frame': boundary, 'silence_frames': playback.silence_frames,
              'next_track_ready_margin_seconds': margin, 'startup_seconds': playback.started - began,
              'elapsed_seconds': playback.elapsed, 'native_checks': native, 'output_checks': converted,
              'conversions': conversions, 'server_requests': records, 'source_sessions': [s.summary() for s in sessions],
              'retries': retries, 'device': getattr(playback, 'device_report', None)}
    (output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
    nav.run(nav.FFMPEG + ['-f', 's32le', '-ar', str(args.output_rate), '-ac', '2', '-i', str(output / 'rendered.s32le'),
                         '-c:a', 'pcm_s24le', str(output / 'listening.wav')])
    print(f'{name}: verified; {playback.silence_frames / args.output_rate:.3f}s starvation; next ready {margin:.3f}s before boundary', flush=True)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('navidrome', 'stratum', 'checker', 'source', 'next-source', 'out'):
        parser.add_argument('--' + name, type=Path, required=True)
    parser.add_argument('--output-rate', type=int, choices=(44100, 48000, 96000), required=True)
    parser.add_argument('--scenario', choices=(*handoff.SCENARIOS, 'all'), default='mixed')
    parser.add_argument('--audio-helper', type=Path)
    parser.add_argument('--audio-device')
    parser.add_argument('--audio-gain', type=float, default=0.25)
    args = parser.parse_args()
    nav.require(bool(args.audio_helper) == bool(args.audio_device) and 0 <= args.audio_gain <= 1, 'invalid output options')
    nav.require(not args.audio_helper or args.scenario != 'all', 'choose one device scenario')
    for name in ('navidrome', 'stratum', 'checker', 'source', 'next_source', 'audio_helper'):
        if getattr(args, name):
            setattr(args, name, getattr(args, name).resolve(strict=True))
    sources = [args.source, args.next_source]
    infos = [handoff.inspect_source(s, args.stratum) for s in sources]
    nav.require(infos[0]['sha256'] != infos[1]['sha256'], 'use different tracks')
    nav.require(infos[0]['frames'] >= 13 * infos[0]['sample_rate'] and infos[1]['frames'] >= 21 * infos[1]['sample_rate'], 'tracks too short')
    args.out.mkdir(parents=True, exist_ok=False)
    args.out = args.out.resolve()
    results = []
    for name in handoff.SCENARIOS if args.scenario == 'all' else (args.scenario,):
        with tempfile.TemporaryDirectory(prefix='stratum-mixed-nav-') as temporary:
            with nav.disposable_library(args.navidrome, sources, Path(temporary)) as (account, songs, _):
                results.append(run_case(name, account, songs, infos, sources, args, args.out / name))
    (args.out / 'report.json').write_text(json.dumps({'experiment': 'stratum-mixed-output-poc-1', 'cases': results,
        'source_info': infos, 'server_stopped': True, 'scope': 'Fixed client output; not bit-perfect; no Astra integration.'}, indent=2) + '\n')
    print(f'Mixed-rate proof passed. Report: {args.out / "report.json"}', flush=True)


if __name__ == '__main__':
    main()
