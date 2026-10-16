import array
from fractions import Fraction
import math
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

import navidrome as nav
from output_clock import Converter, mapping, resample_filter


def signal(rate, frames):
    return array.array('i', (int(0x30000000 * math.sin(i / rate * (1793 if ch else 791) * math.tau))
                            for i in range(frames) for ch in range(2))).tobytes()


def reference(pcm, source_rate, output_rate):
    if source_rate == output_rate:
        return pcm
    result = subprocess.run(nav.FFMPEG + ['-f', 's32le', '-ar', str(source_rate), '-ac', '2', '-i', 'pipe:0',
        '-af', resample_filter(output_rate), '-c:a', 'pcm_s32le', '-f', 's32le', 'pipe:1'],
        input=pcm, capture_output=True, timeout=10)
    if result.returncode:
        raise RuntimeError(result.stderr.decode())
    return result.stdout


class OutputClockTests(unittest.TestCase):
    def test_cumulative_duration_has_no_per_segment_or_per_track_rounding_drift(self):
        infos = [{'sample_rate': r, 'frames': 20 * r + 317} for r in (44100, 48000, 96000)] * 333
        ranges = [{'origin': 1, 'frames': x['sample_rate'] + 1} for x in infos]
        for output in (44100, 48000, 96000):
            parts = mapping(infos, ranges, output)
            duration, previous = Fraction(0), 0
            for info, part in zip(infos, parts):
                duration += Fraction(part['frames'], info['sample_rate'])
                self.assertEqual(part['output_start'], previous)
                previous += part['output_frames']
                self.assertEqual(previous, int(duration * output))
                self.assertLess(duration * output - previous, 1)
                self.assertEqual(part['input_origin'] * output % info['sample_rate'], 0)

    def test_all_clock_pairs_stream_before_eof_and_match_full_source_conversion(self):
        for source in (44100, 48000, 96000):
            full = signal(source, 4 * source + 317)
            for output in (44100, 48000, 96000):
                with self.subTest(source=source, output=output), tempfile.TemporaryDirectory() as folder:
                    part = mapping([{'sample_rate': source, 'frames': len(full) // 8}],
                                   [{'origin': source + 37, 'frames': 2 * source + 7}], output)[0]
                    chunks, emitted = [], 0
                    def send(start, pcm):
                        nonlocal emitted
                        self.assertEqual(start, emitted)
                        emitted += len(pcm) // 8
                        chunks.append(pcm)
                    converter = Converter(part, output, send, Path(folder))
                    try:
                        start = part['input_origin']
                        for count in (1, 7, 101, source + 1):
                            converter.append(start, full[start * 8:(start + count) * 8])
                            start += count
                        self.assertGreater(emitted, output // 2, 'converter buffered whole track')
                        while start < part['input_end']:
                            count = min(source // 3 + 1, part['input_end'] - start)
                            converter.append(start, full[start * 8:(start + count) * 8])
                            start += count
                        report = converter.finish()
                        self.assertEqual(report['input_frames'], part['input_end'] - part['input_origin'])
                        self.assertEqual(emitted, part['output_frames'])
                        converted = reference(full, source, output)
                        offset = part['origin'] * output // source
                        self.assertEqual(b''.join(chunks), converted[offset * 8:(offset + emitted) * 8])
                    finally:
                        converter.close()
                    if converter.process:
                        self.assertIsNotNone(converter.process.poll())

    def test_one_frame_eof_is_drained_without_padding_or_a_reset(self):
        for source, output in ((44100, 48000), (96000, 44100), (48000, 96000)):
            with self.subTest(source=source, output=output), tempfile.TemporaryDirectory() as folder:
                full = signal(source, 2 * source + 1)
                part = mapping([{'sample_rate': source, 'frames': len(full) // 8}],
                               [{'origin': 0, 'frames': len(full) // 8}], output)[0]
                result = []
                converter = Converter(part, output, lambda _, pcm: result.append(pcm), Path(folder))
                try:
                    converter.append(0, full[:-8])
                    process = converter.process
                    converter.append(len(full) // 8 - 1, full[-8:])
                    self.assertIs(converter.process, process)
                    converter.finish()
                    expected = reference(full, source, output)[:part['output_frames'] * 8]
                    self.assertEqual(b''.join(result), expected)
                finally:
                    converter.close()

    def test_resetting_conversion_per_network_segment_is_detectably_wrong(self):
        full = signal(44100, 44100 + 17)
        continuous = reference(full, 44100, 48000)
        split = 22051 * 8
        reset = reference(full[:split], 44100, 48000) + reference(full[split:], 44100, 48000)
        self.assertNotEqual(reset, continuous)

    def test_failed_converter_start_closes_its_resources(self):
        with tempfile.TemporaryDirectory() as folder:
            part = mapping([{'sample_rate': 44100, 'frames': 100000}], [{'origin': 0, 'frames': 88201}], 48000)[0]
            converter = Converter.__new__(Converter)
            with mock.patch('output_clock.subprocess.Popen', side_effect=FileNotFoundError('missing ffmpeg')):
                with self.assertRaises(FileNotFoundError):
                    converter.__init__(part, 48000, lambda *_: None, Path(folder))
            self.assertTrue(converter.closed)
            self.assertTrue(converter.log.closed)
            self.assertIsNone(converter.process)

    def test_rejects_missing_repeated_or_truncated_native_input_and_cleans_up(self):
        with tempfile.TemporaryDirectory() as folder:
            part = mapping([{'sample_rate': 44100, 'frames': 100000}], [{'origin': 0, 'frames': 88201}], 48000)[0]
            converter = Converter(part, 48000, lambda *_: None, Path(folder))
            try:
                with self.assertRaises(RuntimeError):
                    converter.append(1, bytes(8))
                converter.append(0, bytes(8))
                with self.assertRaises(RuntimeError):
                    converter.append(0, bytes(8))
                with self.assertRaises(RuntimeError):
                    converter.finish()
            finally:
                converter.close()
            self.assertIsNotNone(converter.process.poll())
            with self.assertRaises(RuntimeError):
                converter.append(1, bytes(8))


if __name__ == '__main__':
    unittest.main()
