"""Cached bytes must not bypass account/track access or source revision checks."""
import unittest
import urllib.error
import threading
from unittest.mock import patch
import navidrome as nav
from source_session import SourceSession
from test_navidrome import Response


class Account:
    def __init__(self):
        self.allowed = True
        self.size = 8
        self.calls = 0

    def request(self, endpoint, **params):
        self.calls += 1
        if not self.allowed:
            raise RuntimeError("account denied")
        return {"song": {"size": self.size}}

    def url(self, *args, **kwargs):
        return "http://127.0.0.1/source"


def validation(validator='"revision-1"'):
    return Response(b"a", content_range="bytes 0-0/8", validator=validator)


class Sessions(unittest.TestCase):
    def setUp(self):
        self.account = Account()
        self.session = SourceSession(self.account, "track")
        self.blocks = patch.object(nav, "BLOCK_BYTES", 4)
        self.blocks.start()
        self.addCleanup(self.blocks.stop)
        self.addCleanup(self.session.close)

    def warm(self):
        with patch.object(nav.HTTP, "open", side_effect=[validation(), Response()]):
            with self.session.lease(self.account, "track") as source:
                self.assertEqual(source.block(0), b"abcd")

    def test_cached_acquisition_rechecks_metadata_and_actual_stream(self):
        self.warm()
        with patch.object(nav.HTTP, "open", return_value=validation()) as opened:
            with self.session.lease(self.account, "track") as source:
                self.assertEqual(source.block(0), b"abcd")
        self.assertEqual(self.account.calls, 2)
        self.assertEqual(opened.call_count, 1)
        self.assertEqual(opened.call_args.args[0].get_header("Range"), "bytes=0-0")
        self.assertEqual(opened.call_args.args[0].get_header("If-range"), '"revision-1"')
        self.assertEqual(self.session.summary()["upstream_body_bytes"], 6)

    def test_metadata_denial_invalidates_warm_cache(self):
        self.warm()
        self.account.allowed = False
        with patch.object(nav.HTTP, "open") as opened:
            with self.assertRaisesRegex(RuntimeError, "account denied"):
                with self.session.lease(self.account, "track"):
                    self.fail("denied account reached cached bytes")
            opened.assert_not_called()
        self.assertTrue(self.session.closed)
        self.assertFalse(self.session.source.blocks)

    def test_stream_denial_even_after_metadata_success_blocks_cache_hit(self):
        self.warm()
        denied = urllib.error.HTTPError("http://127.0.0.1/source", 403, "denied", {}, None)
        with patch.object(nav.HTTP, "open", side_effect=denied):
            with self.assertRaises(urllib.error.HTTPError):
                with self.session.lease(self.account, "track"):
                    self.fail("stream-denied account reached cached bytes")
        self.assertFalse(self.session.source.blocks)
        with self.assertRaisesRegex(RuntimeError, "invalid"):
            self.session.source.block(0)

    def test_another_account_or_track_cannot_use_warm_session(self):
        self.warm()
        for account, track in [(Account(), "track"), (self.account, "other")]:
            with self.assertRaisesRegex(RuntimeError, "another account or track"):
                with self.session.lease(account, track):
                    self.fail("wrong identity reached cached bytes")

    def test_same_size_replacement_invalidates_all_cached_blocks(self):
        self.warm()
        with patch.object(nav.HTTP, "open", return_value=validation('"revision-2"')):
            with self.assertRaisesRegex(RuntimeError, "changed"):
                with self.session.lease(self.account, "track"):
                    self.fail("new revision reused old bytes")
        self.assertFalse(self.session.source.blocks)
        self.assertTrue(self.session.closed)

    def test_account_handle_with_changed_stream_identity_cannot_reuse_bytes(self):
        self.warm()
        with patch.object(self.account, "url", return_value="http://127.0.0.1/other"), patch.object(nav.HTTP, "open") as opened:
            with self.assertRaisesRegex(RuntimeError, "identity changed"):
                with self.session.lease(self.account, "track"):
                    self.fail("changed connection reused cached bytes")
            opened.assert_not_called()
        self.assertFalse(self.session.source.blocks)

    def test_changed_size_rejected_before_cache_or_stream(self):
        self.warm()
        self.account.size = 9
        with patch.object(nav.HTTP, "open") as opened:
            with self.assertRaisesRegex(RuntimeError, "size changed"):
                with self.session.lease(self.account, "track"):
                    self.fail("new size reused old bytes")
            opened.assert_not_called()
        self.assertFalse(self.session.source.blocks)

    def test_unreachable_or_ignored_range_does_not_fall_back_to_cached_audio(self):
        for response in [OSError("offline"), Response(status=200)]:
            with self.subTest(response=response):
                self.session = SourceSession(self.account, "track")
                self.warm()
                options = {"side_effect": response} if isinstance(response, Exception) else {"return_value": response}
                with patch.object(nav.HTTP, "open", **options):
                    with self.assertRaises((OSError, RuntimeError)):
                        with self.session.lease(self.account, "track"):
                            self.fail("validation failure reused cached bytes")
                self.assertTrue(self.session.closed)
                self.assertFalse(self.session.source.blocks)

    def test_explicit_close_clears_bytes_and_blocks_future_leases(self):
        self.warm()
        self.session.close()
        with self.assertRaisesRegex(RuntimeError, "closed"):
            with self.session.lease(self.account, "track"):
                self.fail("closed session reopened")
        self.assertFalse(self.session.source.blocks)

    def test_lru_limit_survives_multiple_acquisitions(self):
        self.warm()
        with patch.object(nav, "MAX_CACHED_BLOCKS", 1), patch.object(nav.HTTP, "open", side_effect=[
                validation(), Response(b"efgh", content_range="bytes 4-7/8")]):
            with self.session.lease(self.account, "track") as source:
                source.block(1)
            self.assertEqual(list(source.blocks), [1])
            self.assertEqual(self.session.summary()["peak_cached_bytes"], 4)

    def test_concurrent_leases_do_not_share_an_authorization_window(self):
        entered, release, second = threading.Event(), threading.Event(), threading.Event()
        errors = []

        def first():
            try:
                with self.session.lease(self.account, "track"):
                    entered.set()
                    if not release.wait(2):
                        raise RuntimeError("test timed out")
            except BaseException as error:
                errors.append(error)

        def next_request():
            try:
                with self.session.lease(self.account, "track"):
                    second.set()
            except BaseException as error:
                errors.append(error)

        with patch.object(nav.HTTP, "open", side_effect=[validation(), validation()]):
            a = threading.Thread(target=first)
            b = threading.Thread(target=next_request)
            a.start()
            try:
                self.assertTrue(entered.wait(1))
                b.start()
                self.assertFalse(second.wait(0.05))
                self.assertEqual(self.account.calls, 1)
            finally:
                release.set()
                a.join(timeout=2)
                if b.ident is not None:
                    b.join(timeout=2)
            self.assertFalse(a.is_alive() or b.is_alive())
            self.assertFalse(errors)
            self.assertTrue(second.is_set())
            self.assertEqual(self.account.calls, 2)


if __name__ == "__main__":
    unittest.main()
