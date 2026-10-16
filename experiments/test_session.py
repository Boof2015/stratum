"""Long-session lifecycle regressions; no network listeners or audio devices."""
import gc
from pathlib import Path
import tempfile
import threading
import unittest
from unittest.mock import Mock, patch
import weakref

import navidrome as nav
from mixed_seek import TransientStreamError
from seek import Ticket
from seek_output import FileTimeline
from session import retry_fetch, SessionServer, SessionClient
from source_session import SourceSession


class SessionTests(unittest.TestCase):
    def test_incomplete_body_retries_without_admitting_partial_audio(self):
        ticket, records = Ticket(), []
        ticket.cancelled.wait = Mock(return_value=False)
        fetch = Mock(side_effect=[TransientStreamError('partial'), ConnectionResetError(), b'complete'])
        self.assertEqual(retry_fetch(fetch, ticket, records.append), b'complete')
        self.assertEqual([r['delay'] for r in records], [.25, .5])
        self.assertEqual(fetch.call_count, 3)

    def test_integrity_format_and_auth_failures_do_not_retry(self):
        for message in ('corrupt payload', 'response format mismatch', 'segment request failed (401)'):
            with self.subTest(message=message):
                fetch, record = Mock(side_effect=RuntimeError(message)), Mock()
                with self.assertRaisesRegex(RuntimeError, message.replace('(', '\\(').replace(')', '\\)')):
                    retry_fetch(fetch, Ticket(), record)
                fetch.assert_called_once()
                record.assert_not_called()

    def test_cancel_during_backoff_does_not_start_another_request(self):
        ticket = Ticket()
        fetch = Mock(side_effect=TransientStreamError('offline'))
        with self.assertRaises(nav.Cancelled):
            retry_fetch(fetch, ticket, lambda _: ticket.cancel())
        fetch.assert_called_once()

    def test_retry_deadline_stops_before_starting_another_request(self):
        ticket = Ticket()
        ticket.cancelled.wait = Mock()
        fetch = Mock(side_effect=TransientStreamError('offline'))
        with patch('session.time.monotonic', side_effect=[0, 0, .1, .1, 2]):
            with self.assertRaisesRegex(RuntimeError, 'retry budget exhausted'):
                retry_fetch(fetch, ticket, lambda _: None, timeout=1)
        fetch.assert_called_once()

    def test_old_queue_can_be_released_without_losing_capture_counters(self):
        with tempfile.TemporaryDirectory() as temp:
            sink = FileTimeline(Path(temp), 48000)
            old = sink.players[0]
            reference = weakref.ref(old)
            try:
                sink.append(0, 0, bytes(24000 * 8))
                with self.assertRaisesRegex(RuntimeError, 'active consumer'):
                    sink.retire(0)
                sink.switch(1, 1)
                before = sink.snapshot(0)
                sink.retire(0)
                sink.retire(0)  # Idempotent and never touches the current queue.
                self.assertEqual(sink.snapshot(0), before)
                self.assertEqual(len(old.queue), 0)
                self.assertNotIn(0, sink.players)
                self.assertNotIn(0, sink.threads)
                del old
                gc.collect()
                self.assertIsNone(reference())
                sink.append(1, 0, bytes(8))
                sink.finish()
                sink.retire(1)
                sink.switch(2, 1)  # Current generation was already fully retired.
                sink.append(2, 0, bytes(8))
                sink.finish()
            finally:
                sink.stop()

    def test_session_output_rechecks_generation_after_conversion(self):
        client = SessionClient.__new__(SessionClient)
        client.lock = threading.RLock()
        client.generation = 2
        client.output_holds = {}
        client.stale_rejected = 0
        client.sink = Mock()
        handle = Mock()
        with self.assertRaises(nav.Cancelled):
            client.send_part(1, 0, handle, 0, bytes(8))
        self.assertEqual(client.stale_rejected, 1)
        client.sink.append.assert_not_called()
        handle.write.assert_not_called()
        client.sink.append.return_value = True
        client.send_part(2, 0, handle, 0, bytes(8))
        client.sink.append.assert_called_once_with(2, 0, bytes(8))
        handle.write.assert_called_once_with(bytes(8))

    def test_server_retirement_releases_only_its_own_track_caches(self):
        server = SessionServer(Mock(), [], [], Path('unused'), Path('unused'), Mock())
        server.cancel = Mock()
        owned = []
        for gen, track in ((0, 0), (0, 1), (1, 0)):
            session = SourceSession(Mock(), str(track))
            session.source = nav.RangeSource('http://test.invalid', 100)
            session.source.blocks[0] = b'x' * 100
            server.sessions[(gen, track)] = session
            if gen == 0:
                owned.append(session)
        server.records = [{'generation': 0, 'finished': 1}]
        server.retire(0)
        server.cancel.assert_called_once_with(0)
        self.assertEqual(list(server.sessions), [(1, 0)])
        self.assertTrue(all(s.closed and not s.source.blocks for s in owned))
        self.assertEqual(len(server.retired), 2)
        self.assertEqual(server.sessions[(1, 0)].source.blocks[0], b'x' * 100)
        self.assertNotEqual(server.session_key(0, 0), server.session_key(0, 1))
        server.sessions[(1, 0)].close()


if __name__ == '__main__':
    unittest.main()
