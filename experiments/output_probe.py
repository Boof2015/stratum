#!/usr/bin/env python3
"""Muted macOS output regressions: legacy packets, full-queue seek, superseded startup.

An explicit device is required. No Navidrome/network or audible playback.
"""
import argparse
import json
from pathlib import Path
import struct
import subprocess

import navidrome as nav
from seek_output import NativeTimeline


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--helper', type=Path, required=True)
    parser.add_argument('--device', required=True)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    helper = args.helper.resolve(strict=True)
    args.out.mkdir(parents=True, exist_ok=False)
    output = args.out.resolve()
    frames = 48017
    pcm = struct.pack('<ii', 123456, -654321) * frames
    legacy = output / 'legacy'
    legacy.mkdir()
    with subprocess.Popen([str(helper), '--device', args.device, '--out', str(legacy),
                           '--total', str(frames), '--gain', '0'],
                           stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE) as process:
        try:
            stdout, stderr = process.communicate(struct.pack('<QQ', 0, frames) + pcm, timeout=8)
            nav.require(process.returncode == 0, stderr.decode(errors='replace'))
            nav.require(b'"finished"' in stdout, 'legacy helper did not finish')
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=2)
    nav.require((legacy / 'rendered.s32le').read_bytes() == pcm, 'legacy output/tail changed')

    controlled = output / 'controlled'
    controlled.mkdir()
    sink = NativeTimeline(controlled, frames, helper, args.device, 0)
    try:
        sink.append(0, 0, bytes(len(pcm)))
        sink.wait(lambda: sink.states[0]['started'] is not None)
        before = sink.snapshot(0)
        nav.require(before['accepted'] == frames and before['consumed'] < frames, 'full queue was not live at reset')
        sink.switch(1, frames)  # Must still read controls when all PCM is queued.
        sink.switch(2, frames)  # Supersede a generation before its first PCM.
        sink.inject_stale(0)
        sink.append(2, 0, pcm)
        sink.finish()
        nav.require(sink.snapshot(1)['consumed'] == 0, 'superseded startup emitted audio')
        nav.require((controlled / 'generation-2/rendered.s32le').read_bytes() == pcm,
                    'new generation contains stale, missing or repeated PCM')
        nav.require(sink.rejected == [0], 'native stale packet not rejected')
    finally:
        sink.stop()
    result = {'legacy_odd_tail_exact': True, 'fully_queued_seek': True,
              'superseded_startup_silent': True, 'native_stale_rejected': True,
              'new_generation_exact': True, 'gain': 0}
    (output / 'report.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result), flush=True)


if __name__ == '__main__':
    main()
