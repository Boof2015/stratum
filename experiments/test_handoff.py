import copy
from pathlib import Path
import struct
import tempfile
import unittest

import handoff
import live


class HandoffTests(unittest.TestCase):
    def test_high_rate_music_requires_independent_codec_alignment(self):
        checks = {"sample_rate": 96000, "inconclusive": 0, "zero_lag": False,
                  "native_lag_within_codec_frame": True,
                  "nonzero_alignment_windows": [{"quality": "opus-64", "lag": 1}],
                  "lossy_codec_alignment": {"sample_rate": 48000, "evaluable_windows": 6,
                                            "inconclusive": 0, "zero_lag": True}}
        self.assertTrue(handoff.alignment_passes(checks))
        for key, value in (("sample_rate", 48000), ("inconclusive", 1),
                           ("native_lag_within_codec_frame", False),
                           ("nonzero_alignment_windows", [{"quality": "lossless", "lag": 1}]),
                           ("lossy_codec_alignment", {})):
            changed = copy.deepcopy(checks)
            changed[key] = value
            self.assertFalse(handoff.alignment_passes(changed))
        for key, value in (("zero_lag", False), ("inconclusive", 1), ("evaluable_windows", 0),
                           ("sample_rate", 96000)):
            changed = copy.deepcopy(checks)
            changed["lossy_codec_alignment"][key] = value
            self.assertFalse(handoff.alignment_passes(changed))

    def test_96000_clock_keeps_even_conversion_phase_and_odd_eof(self):
        clock = handoff.Clock(96000)
        plans = handoff.intervals([clock.boundary, clock.total - clock.boundary], rate=96000)
        self.assertEqual(plans[6]["frames"], 1)
        self.assertEqual(plans[7]["start"], 1152007)
        self.assertEqual(sum(plan["frames"] for plan in plans), 3072016)
        for start in (0, 1, 2, 3, 150 * 96000 + 37, 150 * 96000 + 38):
            begin, frames, trim = handoff.acquisition_interval(start, 192001, 160 * 96000, 96000)
            self.assertEqual(begin % 2, 0)
            self.assertEqual(begin + trim, start)
            self.assertEqual(begin + frames, start + 192001 + 11520)
        with self.assertRaises(RuntimeError):
            handoff.playback_ranges([{"frames": 160 * 96000, "sample_rate": 96000},
                                    {"frames": 160 * 48000, "sample_rate": 48000}], True)

    def test_96000_sink_and_codec_mapping_use_native_frames(self):
        with tempfile.TemporaryDirectory() as temporary:
            sink = live.Playback(Path(temporary), total=96000, start_buffer=1, rate=96000)
            sink.append(0, bytes(96000 * 8))
            self.assertEqual(sink.buffered(), 1)
            self.assertEqual(sink.quantum, 1920)
        plan = {"track": 1, "source_start": 0, "start": 1152007, "frames": 192001}
        envelope = {**plan, "metadata": {"quality": "opus-64", "sample_rate": 96000, "channels": 2,
                    "presentation": {"frames": 192001}, "encoded_interval": {"frames": 203521},
                    "discard_start_frames": 0, "discard_end_frames": 11521,
                    "decoded_frames": 203522, "codec_sample_rate": 48000,
                    "codec_decoded_frames": 101761, "resample_tail_frames": 1}}
        handoff.validate_envelope(envelope, plan, "opus-64", 96000)
        for key, value in (("codec_sample_rate", 96000), ("codec_decoded_frames", 203522),
                           ("resample_tail_frames", 2), ("sample_rate", 48000)):
            changed = copy.deepcopy(envelope)
            changed["metadata"][key] = value
            with self.assertRaises(RuntimeError):
                handoff.validate_envelope(changed, plan, "opus-64", 96000)

    def test_441_clock_preserves_native_frames_and_rejects_mixed_rates(self):
        clock = handoff.Clock(44100)
        plans = handoff.intervals([clock.boundary, clock.total - clock.boundary], rate=44100)
        self.assertEqual(plans[6]["frames"], 1)
        self.assertEqual(plans[7]["start"], 529207)
        self.assertEqual(sum(plan["frames"] for plan in plans), 1411216)
        begin, frames, trim = handoff.acquisition_interval(150 * 44100 + 37, 88201, 160 * 44100, 44100)
        self.assertEqual(begin % 147, 0)
        self.assertEqual(begin + trim, 150 * 44100 + 37)
        self.assertGreaterEqual(frames - trim, 88201)
        with self.assertRaises(RuntimeError):
            handoff.playback_ranges([{"frames": 160 * 44100, "sample_rate": 44100},
                                    {"frames": 160 * 48000, "sample_rate": 48000}], True)

    def test_441_file_sink_uses_native_clock(self):
        with tempfile.TemporaryDirectory() as temporary:
            sink = live.Playback(Path(temporary), total=44100, start_buffer=1, rate=44100)
            sink.append(0, bytes(44100 * 8))
            self.assertEqual(sink.buffered(), 1)
            self.assertEqual(sink.quantum, 882)

    def test_441_response_rejects_wrong_codec_clock_and_rounding_tail(self):
        plan = {"track": 1, "source_start": 0, "start": 529207, "frames": 88201}
        meta = {"quality": "opus-64", "sample_rate": 44100, "channels": 2,
                "presentation": {"frames": 88201}, "encoded_interval": {"frames": 93493},
                "discard_start_frames": 0, "discard_end_frames": 5293,
                "decoded_frames": 93494, "codec_sample_rate": 48000,
                "codec_decoded_frames": 101762, "resample_tail_frames": 1}
        envelope = {**plan, "metadata": meta}
        handoff.validate_envelope(envelope, plan, "opus-64", 44100)
        for key, value in (("codec_sample_rate", 44100), ("codec_decoded_frames", 93494),
                           ("resample_tail_frames", 0), ("sample_rate", 48000)):
            changed = copy.deepcopy(envelope)
            changed["metadata"][key] = value
            with self.assertRaises(RuntimeError):
                handoff.validate_envelope(changed, plan, "opus-64", 44100)
        for delta in (-2, 1):
            changed = copy.deepcopy(envelope)
            for key in ("decoded_frames", "resample_tail_frames", "discard_end_frames"):
                changed["metadata"][key] += delta
            with self.assertRaises(RuntimeError):
                handoff.validate_envelope(changed, plan, "opus-64", 44100)

    def test_actual_pair_reaches_original_eof_and_starts_next_at_zero(self):
        ranges = handoff.playback_ranges([{"frames": 10275429}, {"frames": 11415273}], True)
        plans = handoff.intervals([part["frames"] for part in ranges], [part["origin"] for part in ranges])
        first = [plan for plan in plans if plan["track"] == 0]
        second = [plan for plan in plans if plan["track"] == 1]
        self.assertEqual(first[0]["source_start"], 10275429 - handoff.BOUNDARY)
        self.assertEqual(first[-1]["source_start"] + first[-1]["frames"], 10275429)
        self.assertEqual(second[0]["source_start"], 0)
        self.assertEqual(second[0]["start"], handoff.BOUNDARY)
        self.assertEqual(sum(plan["frames"] for plan in plans), handoff.TOTAL)
        with self.assertRaises(RuntimeError):
            handoff.playback_ranges([{"frames": handoff.BOUNDARY - 1}, {"frames": handoff.TOTAL}], True)

    def test_plan_keeps_one_frame_eof_then_starts_next_at_zero(self):
        plans = handoff.intervals([handoff.BOUNDARY, handoff.TOTAL - handoff.BOUNDARY])
        before, after = plans[6:8]
        self.assertEqual(before["frames"], 1)
        self.assertEqual(before["source_start"] + before["frames"], handoff.BOUNDARY)
        self.assertEqual(after["source_start"], 0)
        self.assertEqual(after["start"], handoff.BOUNDARY)
        self.assertEqual(sum(plan["frames"] for plan in plans), handoff.TOTAL)

    def test_context_clamps_to_own_track_start_and_end(self):
        self.assertEqual(handoff.acquisition_interval(0, 96001, 200000), (0, 101761, 0))
        self.assertEqual(handoff.acquisition_interval(199999, 1, 200000), (194239, 5761, 5760))
        self.assertEqual(handoff.acquisition_interval(0, 16, 16), (0, 16, 0))
        for args in ((-1, 1, 16), (16, 1, 16), (15, 2, 16), (0, 0, 16)):
            with self.assertRaises(RuntimeError):
                handoff.acquisition_interval(*args)

    def test_response_cannot_use_previous_track_or_full_segment_for_short_tail(self):
        plan = {"track": 1, "source_start": 0, "start": handoff.BOUNDARY, "frames": 1}
        envelope = {**plan, "metadata": {"quality": "opus-64", "sample_rate": 48000, "channels": 2,
                    "presentation": {"frames": 1}, "discard_start_frames": 0,
                    "discard_end_frames": 5759, "decoded_frames": 5760}}
        handoff.validate_envelope(envelope, plan, "opus-64")
        for key, value in (("track", 0), ("source_start", 96001), ("start", 0), ("frames", 96001)):
            changed = copy.deepcopy(envelope)
            changed[key] = value
            with self.assertRaises(RuntimeError):
                handoff.validate_envelope(changed, plan, "opus-64")
        changed = copy.deepcopy(envelope)
        changed["metadata"]["discard_end_frames"] = -1
        with self.assertRaises(RuntimeError):
            handoff.validate_envelope(changed, plan, "opus-64")

    def test_consumer_crosses_track_boundary_inside_one_quantum(self):
        # Different values expose dropped/repeated track-edge samples.
        a = struct.pack("<ii", 111, -111) * 961
        b = struct.pack("<ii", 222, -222) * 977
        with tempfile.TemporaryDirectory() as temporary:
            sink = live.Playback(Path(temporary), total=1938, start_buffer=1)
            sink.append(0, a)
            sink.append(961, b)
            chunks = [sink.consume(960)[0] for _ in range(3)]
            self.assertEqual(b"".join(chunks), a + b)
            self.assertEqual(sink.consumed, 1938)

    def test_late_next_track_starves_without_repeating_tail_or_advancing(self):
        with tempfile.TemporaryDirectory() as temporary:
            sink = live.Playback(Path(temporary), total=1921, start_buffer=1)
            a = struct.pack("<ii", 111, -111) * 961
            b = struct.pack("<ii", 222, -222) * 960
            sink.append(0, a)
            first, count = sink.consume(960)
            self.assertEqual((first, count), (a[:960 * 8], 960))
            self.assertEqual(sink.consume(960), (None, 960))
            self.assertEqual(sink.consumed, 960)
            sink.append(961, b)
            second, _ = sink.consume(960)
            final, _ = sink.consume(960)
            self.assertEqual(first + second + final, a + b)


if __name__ == "__main__":
    unittest.main()
