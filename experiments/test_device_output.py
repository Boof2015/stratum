"""Helper failure/cleanup checks using real pipes, without opening an audio device."""
from pathlib import Path
import sys
import tempfile
import threading
import unittest

from device_output import DevicePlayback


class DeviceFailures(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="stratum-device-test-")
        self.addCleanup(self.temp.cleanup)
        self.output = Path(self.temp.name)

    def start(self, script):
        helper = self.output / "fake-output"
        helper.write_text("#!" + sys.executable + "\nimport json, sys, time\n" + script)
        helper.chmod(0o700)
        player = DevicePlayback(self.output, helper, "speakers", 16, 0)
        thread = threading.Thread(target=player.play)
        thread.start()
        def cleanup():
            player.stop()
            thread.join(timeout=4)
            self.assertFalse(thread.is_alive(), "adapter left a reader running")
            self.assertIsNotNone(player.process.poll(), "adapter left a helper running")
        self.addCleanup(cleanup)
        return player

    def finished_with(self, player, message):
        self.assertTrue(player.done.wait(4))
        self.assertRegex(str(player.error), message)
        with self.assertRaisesRegex(RuntimeError, message):
            player.append(0, bytes(8))

    def test_wrong_device_cannot_silently_replace_selected_output(self):
        player = self.start('print(json.dumps({"event":"opened", "device":{"uid":"other"}}), flush=True)\ntime.sleep(30)\n')
        self.finished_with(player, "another output")

    def test_device_open_failure_reaches_waiting_producer(self):
        player = self.start('print("selected output disconnected", file=sys.stderr)\nsys.exit(1)\n')
        self.finished_with(player, "selected output disconnected")

    def test_invalid_consumption_cannot_claim_buffered_frames(self):
        player = self.start('print(json.dumps({"event":"status", "accepted":0, "consumed":1}), flush=True)\ntime.sleep(30)\n')
        self.finished_with(player, "invalid audio queue counters")
        self.assertEqual(player.buffered(), 0)

    def test_cancellation_stops_helper_while_status_reader_is_blocked(self):
        player = self.start('print(json.dumps({"event":"opened", "device":{"uid":"speakers"}}), flush=True)\ntime.sleep(30)\n')
        self.assertTrue(player.ready.wait(4))
        player.stop()
        self.finished_with(player, "audio output failed")


if __name__ == "__main__":
    unittest.main()
