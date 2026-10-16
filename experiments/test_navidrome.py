"""Network-free regression checks for bounded upstream reads.

Run: python3 -B -m unittest discover -s experiments -p 'test_*.py'
The separate navidrome.py experiment exercises the real server and decoder.
"""
import io
import unittest
from unittest.mock import patch
import navidrome as probe


class Response(io.BytesIO):
    def __init__(self, data=b"abcd", status=206, content_range="bytes 0-3/8",
                 validator='"revision-1"', encoding=None):
        super().__init__(data)
        self.status = status
        self.headers = {"Content-Range": content_range}
        if validator is not None:
            self.headers["ETag"] = validator
        if encoding:
            self.headers["Content-Encoding"] = encoding


class Ranges(unittest.TestCase):
    def setUp(self):
        self.block_patch = patch.object(probe, "BLOCK_BYTES", 4)
        self.block_patch.start()
        self.addCleanup(self.block_patch.stop)

    def test_bounded_ranges_reuse_blocks(self):
        source = probe.RangeSource("http://127.0.0.1/source", 8)
        with patch.object(probe.HTTP, "open", side_effect=[Response(), Response(b"efgh", content_range="bytes 4-7/8")]) as opener:
            self.assertEqual(source.block(0), b"abcd")
            self.assertEqual(source.block(0), b"abcd")
            self.assertEqual(source.block(1), b"efgh")
        self.assertEqual(opener.call_count, 2)
        self.assertEqual(opener.call_args_list[0].args[0].get_header("Range"), "bytes=0-3")
        self.assertEqual(opener.call_args_list[1].args[0].get_header("If-range"), '"revision-1"')
        self.assertEqual(sum(r["body_bytes_received"] for r in source.records), 8)
        self.assertEqual(source.hits, 1)

    def test_partial_final_block(self):
        source = probe.RangeSource("http://127.0.0.1/source", 6)
        with patch.object(probe.HTTP, "open", return_value=Response(b"ef", content_range="bytes 4-5/6")) as opener:
            self.assertEqual(source.block(1), b"ef")
        self.assertEqual(opener.call_args.args[0].get_header("Range"), "bytes=4-5")

    def test_invalid_responses_are_not_cached(self):
        cases = [Response(status=200), Response(content_range="bytes 1-4/8"),
                 Response(content_range="bytes 0-3/9"), Response(b"abc"),
                 Response(b"abcde"), Response(validator=None), Response(encoding="gzip")]
        for response in cases:
            with self.subTest(headers=response.headers, status=response.status):
                source = probe.RangeSource("http://127.0.0.1/source", 8)
                with patch.object(probe.HTTP, "open", return_value=response):
                    with self.assertRaises(RuntimeError):
                        source.block(0)
                self.assertFalse(source.blocks)

    def test_source_change_fails_before_caching_new_block(self):
        source = probe.RangeSource("http://127.0.0.1/source", 8)
        with patch.object(probe.HTTP, "open", side_effect=[Response(), Response(b"efgh", content_range="bytes 4-7/8", validator='"revision-2"')]):
            source.block(0)
            with self.assertRaisesRegex(RuntimeError, "changed"):
                source.block(1)
        self.assertNotIn(1, source.blocks)

    def test_weak_etag_uses_last_modified(self):
        response = Response(validator='W/"weak"')
        response.headers["Last-Modified"] = "Fri, 16 Oct 2026 04:00:00 GMT"
        source = probe.RangeSource("http://127.0.0.1/source", 8)
        with patch.object(probe.HTTP, "open", return_value=response):
            source.block(0)
        self.assertEqual(source.validator, response.headers["Last-Modified"])

    def test_cache_evicts_least_recent_block(self):
        source = probe.RangeSource("http://127.0.0.1/source", 12)
        with patch.object(probe, "MAX_CACHED_BLOCKS", 2), patch.object(probe.HTTP, "open", side_effect=[
                Response(content_range="bytes 0-3/12"), Response(b"efgh", content_range="bytes 4-7/12"),
                Response(b"ijkl", content_range="bytes 8-11/12")]):
            source.block(0)
            source.block(1)
            source.block(0)
            source.block(2)
        self.assertEqual(list(source.blocks), [0, 2])

    def test_permission_failure_precedes_any_stream_request(self):
        class Denied:
            def request(self, endpoint, **params):
                raise RuntimeError("Navidrome refused request (Subsonic 40)")
        with patch.object(probe.HTTP, "open") as opener:
            with self.assertRaisesRegex(RuntimeError, "Subsonic 40"):
                with probe.metered_source(Denied(), "track"):
                    self.fail("should not open gateway")
        opener.assert_not_called()


if __name__ == "__main__":
    unittest.main()
