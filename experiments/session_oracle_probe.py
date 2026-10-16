#!/usr/bin/env python3
"""Negative controls for interrupted multi-track output and outage accounting."""
import argparse
import copy
import json
from pathlib import Path
import shutil
import tempfile

from session import verify


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--run', type=Path, required=True)
    parser.add_argument('--checker', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    report = json.loads((args.run / 'report.json').read_text())
    first = json.loads((args.run / 'generation-0/delivery.json').read_text())
    sources = [Path(t['source']) for t in first['tracks'][:3]]
    outcomes = []
    for name in ('hidden-starvation', 'repeated-after-outage', 'skipped-unfinished-track',
                 'coherent-output-shift', 'coherent-old-track'):
        gen = 0 if name in ('hidden-starvation', 'repeated-after-outage', 'skipped-unfinished-track') else 2
        with tempfile.TemporaryDirectory(prefix='stratum-session-oracle-') as temporary:
            folder = Path(temporary) / 'generation'
            shutil.copytree(args.run / f'generation-{gen}', folder)
            plan = copy.deepcopy(report['generations'][gen])
            snapshot = copy.deepcopy(plan)
            capture = folder / 'rendered.s32le'
            if name == 'hidden-starvation':
                snapshot['underruns'] = []
            elif name == 'repeated-after-outage':
                gap = snapshot['underruns'][0]
                at = (gap['at_media_frame'] + gap['silence_frames']) * 8
                data = capture.read_bytes()
                capture.write_bytes(data[:at] + data[at - 8:at] + data[at:-8])
            elif name == 'skipped-unfinished-track':
                part = folder / 'part-0/output.s32le'
                part.write_bytes(part.read_bytes()[:-8])
                snapshot['accepted'] -= 1
            else:
                part = folder / 'part-0/output.s32le'
                data = part.read_bytes()
                if name == 'coherent-output-shift':
                    changed = data[8:] + data[:8]
                else:
                    old = (args.run / 'generation-0/part-0/output.s32le').read_bytes()[:8192]
                    changed = old + data[len(old):]
                part.write_bytes(changed)
                capture.write_bytes(changed)
            try:
                verify(folder, plan, snapshot, sources, report['output_rate'], args.checker.resolve())
            except RuntimeError as error:
                outcomes.append({'case': name, 'rejected': True, 'reason': str(error)[:400]})
            else:
                raise AssertionError('corruption accepted: ' + name)
    args.out.write_text(json.dumps(outcomes, indent=2) + '\n')
    print('All five session corruptions rejected.')


if __name__ == '__main__':
    main()
