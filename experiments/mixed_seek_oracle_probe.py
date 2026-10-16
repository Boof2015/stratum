#!/usr/bin/env python3
"""Deliberately corrupt a completed mixed-seek capture; every copy must fail."""
import argparse
import copy
import json
from pathlib import Path
import shutil
import tempfile

from mixed_seek import verify_generation
import navidrome as nav


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--run', type=Path, required=True)
    parser.add_argument('--checker', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    report = json.loads((args.run / 'report.json').read_text())
    passed = []
    for name in ('wrong-track', 'wrong-native-position', 'native-sample-shift',
                 'coherent-output-shift', 'coherent-old-output', 'capture-short', 'output-short'):
        with tempfile.TemporaryDirectory(prefix='stratum-seek-oracle-') as temporary:
            folder = Path(temporary) / 'generation'
            generation = 0 if name in ('native-sample-shift', 'wrong-native-position') else 4
            shutil.copytree(args.run / f'generation-{generation}', folder)
            plan = copy.deepcopy(report['generations'][generation])
            snapshot = copy.deepcopy(plan)
            source = Path(report['audio_checks'][generation]['source'])
            if name == 'wrong-track':
                source = Path(report['audio_checks'][0]['source'])
            elif name == 'wrong-native-position':
                plan['segments'][0]['source_start'] += 1
            elif name == 'native-sample-shift':
                path = folder / plan['segments'][1]['pcm']
                data = path.read_bytes()
                path.write_bytes(data[8:] + data[:8])
            elif name == 'coherent-output-shift':
                data = (folder / 'output.s32le').read_bytes()
                data = data[8:] + data[:8]
                (folder / 'output.s32le').write_bytes(data)
                (folder / 'rendered.s32le').write_bytes(data)
            elif name == 'coherent-old-output':
                data = (folder / 'output.s32le').read_bytes()
                old = (args.run / 'generation-0/output.s32le').read_bytes()[:8192]
                data = old + data[len(old):]
                (folder / 'output.s32le').write_bytes(data)
                (folder / 'rendered.s32le').write_bytes(data)
            else:
                path = folder / ('rendered.s32le' if name == 'capture-short' else 'output.s32le')
                path.write_bytes(path.read_bytes()[:-8])
            try:
                verify_generation(folder, plan, snapshot, source, report['output_rate'], args.checker.resolve())
            except RuntimeError as error:
                passed.append({'case': name, 'rejected': True, 'reason': str(error)[:400]})
            else:
                raise AssertionError(f'corruption accepted: {name}')
    args.out.write_text(json.dumps(passed, indent=2) + '\n')
    nav.require(len(passed) == 7, 'missing oracle control')
    print('All seven corruptions rejected.')


if __name__ == '__main__':
    main()
