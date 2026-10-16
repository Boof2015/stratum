import copy
from pathlib import Path
import struct
import tempfile
import unittest

import handoff
import live


class HandoffTests(unittest.TestCase):
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
