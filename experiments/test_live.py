"""Network-free checks for timing policy and the real-time consumer's queue."""
from pathlib import Path
import unittest
from unittest.mock import patch
import live


class Delivery(unittest.TestCase):
    def test_partial_download_retries_exact_interval_before_deadline(self):
        policy = live.Policy(440000)
        self.assertFalse(policy.endangered(8192, 440000, 0.4, 6))
        self.assertTrue(policy.endangered(16384, 440000, 0.7, 6))
        self.assertEqual(policy.quality, "opus-64")
        self.assertFalse(policy.endangered(16384, 20000, 1, 0.01))

    def test_short_slow_blip_does_not_abandon_nearly_complete_segment(self):
        policy = live.Policy(440000)
        self.assertFalse(policy.endangered(430000, 440000, 1.5, 5))

    def test_recovery_needs_repeated_evidence_and_buffer(self):
        policy = live.Policy(440000)
        policy.endangered(16384, 440000, 0.7, 6)
        for _ in range(2):
            policy.observe(20000, 0.02, 0.6)
            self.assertEqual(policy.choose(6), "opus-64")
        policy.observe(20000, 0.02, 0.6)
        self.assertEqual(policy.choose(4), "opus-64")
        self.assertEqual(policy.choose(6), "lossless")

    def test_low_bandwidth_stays_sustainable(self):
        policy = live.Policy(440000)
        policy.observe(20000, 20000 / 24000, 0.65)
        self.assertEqual(policy.choose(6), "opus-64")
        for _ in range(8):
            policy.observe(20000, 20000 / 24000, 0.65)
            self.assertEqual(policy.choose(8), "opus-64")

    def test_late_data_cannot_advance_or_skip_media_clock(self):
        player = live.Playback(Path("unused"))
        self.assertEqual(player.consume(2), (None, 2))
        self.assertEqual(player.consumed, 0)
        player.append(0, b"a" * live.FRAME_BYTES)
        self.assertEqual(player.consume(2), (None, 2))
        player.append(1, b"b" * live.FRAME_BYTES)
        self.assertEqual(player.consume(2), (b"a" * live.FRAME_BYTES + b"b" * live.FRAME_BYTES, 2))
        self.assertEqual(player.consumed, 2)

    def test_out_of_order_or_duplicate_intervals_rejected(self):
        player = live.Playback(Path("unused"))
        with self.assertRaisesRegex(RuntimeError, "non-contiguous"):
            player.append(1, bytes(live.FRAME_BYTES))
        player.append(0, bytes(live.FRAME_BYTES))
        with self.assertRaisesRegex(RuntimeError, "non-contiguous"):
            player.append(0, bytes(live.FRAME_BYTES))

    def test_late_wakeup_cannot_hide_a_missed_deadline(self):
        player = live.Playback(Path("unused"))
        with patch.object(live.time, "monotonic", return_value=12):
            player.append(0, bytes(live.FRAME_BYTES))
        self.assertEqual(player.consume(1, deadline=11), (None, 1))
        self.assertEqual(player.consumed, 0)
        self.assertEqual(player.consume(1, deadline=12), (bytes(live.FRAME_BYTES), 1))

    def test_profile_uses_wall_clock_not_requested_quality(self):
        player = live.Playback(Path("unused"))
        link = live.Link(player, True)
        player.started = 10
        self.assertEqual(link.rate(14.99), 1250000)
        self.assertEqual(link.rate(15), 24000)
        self.assertEqual(link.rate(27), 1250000)
        self.assertEqual(live.Link(player, False).rate(15), 1250000)


if __name__ == "__main__":
    unittest.main()
