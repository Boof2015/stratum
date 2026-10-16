#!/usr/bin/env python3
"""Silent negative controls for a completed all-lossless mixed-rate delivery."""
import argparse
import copy
import json
from pathlib import Path
import shutil
import tempfile

import mixed_handoff
import navidrome as nav


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--checker', type=Path, required=True)
    parser.add_argument('--delivery', type=Path, required=True)
    args = parser.parse_args()
    source = args.delivery.resolve(strict=True)
    delivery = json.loads(source.read_text())
    nav.require(all(s['quality'] == 'lossless' for s in delivery['segments']), 'use lossless case')
    nav.require(not delivery['underruns'], 'use non-starved case')
    with tempfile.TemporaryDirectory(prefix='stratum-mixed-oracle-') as temporary:
        folder = Path(temporary)
        for name in ['rendered.s32le'] + [s['pcm'] for s in delivery['segments']] + [t['output_pcm'] for t in delivery['tracks']]:
            target = folder / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source.parent / name, target)
        def check(value):
            (folder / 'delivery.json').write_text(json.dumps(value))
            return json.loads(nav.run([str(args.checker.resolve()), str(folder / 'delivery.json')]))
        good = check(delivery)
        nav.require(good['rendered_matches'] and all(t['lossless_exact'] for t in good['tracks']), 'baseline failed')
        mixed_handoff.verify_output(folder, delivery)
        rejected = []
        for label, mutate in (
            ('wrong output duration', lambda d: d['tracks'][1].update(output_frames=d['tracks'][1]['output_frames'] + 1)),
            ('wrong output seek phase', lambda d: d['tracks'][0].update(discard_output_start=d['tracks'][0]['discard_output_start'] + 1)),
            ('missing native frame', lambda d: d['segments'][1].update(source_start=d['segments'][1]['source_start'] + 1)),
            ('wrong source clock', lambda d: d['tracks'][0].update(sample_rate=96000)),
            ('cross-track reordering', lambda d: d['segments'].reverse()),
        ):
            bad = copy.deepcopy(delivery)
            mutate(bad)
            try:
                check(bad)
            except RuntimeError:
                rejected.append(label)
            else:
                raise RuntimeError('oracle accepted ' + label)
        raw = folder / 'rendered.s32le'
        original = raw.read_bytes()
        boundary = delivery['tracks'][1]['output_start'] * 8
        for label, data in (
            ('silence at join', original[:boundary] + bytes(8) + original[boundary + 8:]),
            ('repeated output frame', original[:boundary] + original[boundary - 8:boundary] + original[boundary:-8]),
            ('missing output frame', original[:boundary - 8] + original[boundary:] + bytes(8)),
        ):
            raw.write_bytes(data)
            nav.require(not check(delivery)['rendered_matches'], 'oracle accepted ' + label)
            rejected.append(label)
        raw.write_bytes(original)
        # Self-comparing changed output and capture is insufficient. The separate
        # continuous conversion of native PCM must catch the shared corruption.
        output_path = folder / delivery['tracks'][1]['output_pcm']
        saved_output = output_path.read_bytes()
        changed = bytearray(saved_output)
        changed[1] ^= 1
        output_path.write_bytes(changed)
        bad_capture = bytearray(original)
        bad_capture[boundary + 1] ^= 1
        raw.write_bytes(bad_capture)
        nav.require(check(delivery)['rendered_matches'], 'control must defeat capture self-comparison')
        try:
            mixed_handoff.verify_output(folder, delivery)
        except RuntimeError:
            rejected.append('matching output/capture corruption')
        else:
            raise RuntimeError('continuous reference missed output corruption')
        output_path.write_bytes(saved_output)
        raw.write_bytes(original)
        native_path = folder / delivery['segments'][0]['pcm']
        native = bytearray(native_path.read_bytes())
        native[1] ^= 1
        native_path.write_bytes(native)
        nav.require(not check(delivery)['tracks'][0]['lossless_exact'], 'original reference missed native corruption')
        rejected.append('native lossless corruption')
    print(json.dumps({'baseline_passed': True, 'rejected': rejected}, indent=2))


if __name__ == '__main__':
    main()
