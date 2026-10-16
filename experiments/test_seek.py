"""Cancellation/admission regressions. No audio devices or network listeners."""
import json
import io
import os
from pathlib import Path
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import Mock, patch

import live
import navidrome as nav
from seek import Client, Ticket, validate_envelope


class SeekTests(unittest.TestCase):
    def test_tag_based_server_paths_cannot_swap_fixture_identity(self):
        with tempfile.TemporaryDirectory() as folder:
            sources = [Path(folder) / 'first.flac', Path(folder) / 'second.flac']
            payloads = {'a': b'A' * 1000, 'b': b'B' * 1000}
            for source, body in zip(sources, payloads.values()):
                source.write_bytes(body)
            songs = [{'id': 'b', 'path': 'artist/album/second.flac', 'size': 1000},
                     {'id': 'a', 'path': 'different/virtual/path.flac', 'size': 1000}]
            account = Mock()
            account.url.side_effect = lambda _, **kwargs: 'http://test.invalid/' + kwargs['id']
            def opened(request, timeout):
                body = payloads[request.full_url.rsplit('/', 1)[1]]
                response = io.BytesIO(body)
                response.status = 206
                response.headers = {'Content-Range': 'bytes 0-999/1000'}
                return response
            with patch.object(nav.HTTP, 'open', side_effect=opened):
                ordered = nav.identify_test_songs(account, sources, songs)
                self.assertEqual([song['id'] for song in ordered], ['a', 'b'])
                sources[1].write_bytes(payloads['a'])
                with self.assertRaisesRegex(RuntimeError, 'mapping ambiguous'):
                    nav.identify_test_songs(account, sources, songs)

    def test_cancelling_before_startup_does_not_consume_or_start_audio(self):
        with tempfile.TemporaryDirectory() as folder:
            player = live.Playback(Path(folder))
            player.cancel.set()
            player.ready.set()
            player.play()
            self.assertIsNone(player.started)
            self.assertEqual(player.consumed, 0)
            self.assertTrue(player.done.is_set())

    def test_socket_connected_after_cancellation_is_closed(self):
        ticket, sock = Ticket(), Mock()
        ticket.cancel()
        with self.assertRaises(nav.Cancelled):
            ticket.attach(sock)
        sock.shutdown.assert_called_once()

    def test_cancel_interrupts_registered_socket(self):
        ticket, sock = Ticket(), Mock()
        ticket.attach(sock)
        ticket.cancel()
        sock.shutdown.assert_called_once()
        with self.assertRaises(nav.Cancelled):
            ticket.check()

    def test_late_completed_result_changes_neither_audio_nor_quality_policy(self):
        sink = Mock()
        client = Client(None, sink, Path('unused'), [{"bytes": 40000000, "frames": 160 * live.RATE}])
        client.generation = 3
        client.plans[3] = {"segments": []}
        client.policy = Mock()
        self.assertFalse(client.deliver(1, 0, b'old audio', {}, (1, 1, 1)))
        self.assertEqual(client.stale_rejected, 1)
        sink.append.assert_not_called()
        client.policy.observe.assert_not_called()
        self.assertEqual(client.plans[3]["segments"], [])
        sink.append.return_value = True
        segment = {"start": 0}
        self.assertTrue(client.deliver(3, 0, b'current audio', segment, (100, 1, 0.5)))
        sink.append.assert_called_once_with(3, 0, b'current audio')
        client.policy.observe.assert_called_once_with(100, 1, 0.5)

    def test_wrong_generation_track_or_position_cannot_be_decoded_as_current(self):
        envelope = {"generation": 4, "track": 1, "source_start": 960197, "frames": live.SEGMENT,
                    "metadata": {"quality": "opus-64", "sample_rate": 48000, "channels": 2,
                    "presentation": {"frames": live.SEGMENT}, "discard_start_frames": 5760,
                    "discard_end_frames": 5760, "decoded_frames": live.SEGMENT + 11520}}
        validate_envelope(envelope, 4, 1, 960197, "opus-64")
        for field, value in (("generation", 3), ("track", 0), ("source_start", 960196)):
            with self.assertRaisesRegex(RuntimeError, "another playback position"):
                validate_envelope({**envelope, field: value}, 4, 1, 960197, "opus-64")

    def test_cancel_stops_real_process_before_its_normal_completion(self):
        with tempfile.TemporaryDirectory(prefix='stratum-cancel-') as folder:
            marker = Path(folder) / 'started'
            cancel, errors = threading.Event(), []
            code = 'import os,time,pathlib;pathlib.Path(' + repr(str(marker)) + ').write_text(str(os.getpid()));time.sleep(20)'
            def run():
                try:
                    nav.run([sys.executable, '-c', code], cancel=cancel)
                except BaseException as error:
                    errors.append(error)
            worker = threading.Thread(target=run)
            worker.start()
            try:
                deadline = time.monotonic() + 3
                while not marker.exists() and time.monotonic() < deadline:
                    time.sleep(0.01)
                self.assertTrue(marker.exists())
                pid = int(marker.read_text())
                cancel.set()
                worker.join(timeout=3)
                self.assertFalse(worker.is_alive())
                self.assertEqual(len(errors), 1)
                self.assertIsInstance(errors[0], nav.Cancelled)
                with self.assertRaises(ProcessLookupError):
                    os.kill(pid, 0)
            finally:
                cancel.set()
                worker.join(timeout=5)


if __name__ == '__main__':
    unittest.main()
