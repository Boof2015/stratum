"""Experimental private source session; never a cross-user or persistent cache.

Pins the upstream-reported validator only. Navidrome 0.64.2's Last-Modified comes
from scanned database metadata and may miss file changes before a scan. This is
not a cryptographic source revision or a production consistency guarantee.
"""
import contextlib
import threading
import navidrome as nav


class SourceSession:
    def __init__(self, account, song_id):
        self.account, self.song_id = account, song_id
        self.source = None
        self.lock = threading.Lock()
        self.closed = False
        self.authorizations = 0
        self.record_start = self.hit_start = 0

    @contextlib.contextmanager
    def lease(self, account, song_id):
        with self.lock:
            nav.require(account is self.account and song_id == self.song_id,
                        "source session belongs to another account or track")
            nav.require(not self.closed, "source session is closed")
            try:
                self.authorizations += 1
                song = account.request("getSong", id=song_id)["song"]
                url = account.url("stream", id=song_id, format="raw", maxBitRate=0)
                size = int(song["size"])
                nav.require(0 < size <= nav.MAX_BYTES, "invalid source size")
                if self.source is None:
                    self.source = nav.RangeSource(url, size)
                else:
                    nav.require(url == self.source.url, "source session identity changed")
                    nav.require(size == self.source.size, "source size changed")
                self.record_start = len(self.source.records)
                self.hit_start = self.source.hits
                self.source.revalidate()
                yield self.source
            except BaseException:
                self._close()
                raise

    def _close(self):
        self.closed = True
        if self.source is not None:
            self.source.invalidate()

    def close(self):
        with self.lock:
            self._close()

    def summary(self):
        with self.lock:
            records = self.source.records if self.source else []
            return {"authorizations": self.authorizations,
                    "validation_requests": sum(r["purpose"] == "validation" for r in records),
                    "upstream_body_bytes": sum(r["body_bytes_received"] for r in records),
                    "block_requests": sum(r["purpose"] == "block" for r in records),
                    "block_cache_hits": self.source.hits if self.source else 0,
                    "peak_cached_bytes": self.source.peak_cached_bytes if self.source else 0,
                    "cache_limit_bytes": nav.MAX_CACHED_BLOCKS * nav.BLOCK_BYTES,
                    "closed": self.closed}
