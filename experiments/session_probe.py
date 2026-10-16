#!/usr/bin/env python3
"""Real disposable-Navidrome checks for warm source reuse and rejection paths."""
import argparse
import json
import os
from pathlib import Path
import tempfile
import time

import navidrome as nav
from source_session import SourceSession


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("navidrome", "fixture", "out"):
        parser.add_argument("--" + name, required=True, type=Path)
    args = parser.parse_args()
    args.navidrome = args.navidrome.resolve(strict=True)
    args.fixture = args.fixture.resolve(strict=True)
    args.out.mkdir(parents=True, exist_ok=False)
    args.out = args.out.resolve()
    report = {}
    start, frames = 80 * nav.RATE + 37, 96001
    with tempfile.TemporaryDirectory(prefix="stratum-session-probe-") as directory:
        state = Path(directory)
        with nav.disposable(args.navidrome, args.fixture, state) as (account, song, bad_auth):
            session = SourceSession(account, song["id"])
            try:
                report["cold"] = nav.acquire(account, song["id"], start, frames,
                                              args.out / "cold.flac", session=session)
                report["warm"] = nav.acquire(account, song["id"], start, frames,
                                              args.out / "warm.flac", session=session)
                reference = nav.decode(args.fixture)
                expected = reference[start * nav.CHANNELS * 4:(start + frames) * nav.CHANNELS * 4]
                nav.require(nav.decode(args.out / "cold.flac") == expected and
                            nav.decode(args.out / "warm.flac") == expected, "source PCM mismatch")
                nav.require(report["warm"]["body_bytes_received"] == 1,
                            "repeated interval fetched more than the validation byte")
                report["cold_and_warm_pcm_exact"] = True
                # Simulate credentials becoming unusable on the existing private
                # session. This is not a test of Navidrome's library ACL editor.
                good_auth = account.auth
                account.auth = bad_auth.auth
                try:
                    nav.acquire(account, song["id"], start, frames,
                                args.out / "denied.flac", session=session)
                    raise AssertionError("warm cache bypassed rejected credentials")
                except RuntimeError as error:
                    nav.require("Subsonic 40" in str(error), "unexpected auth failure")
                finally:
                    account.auth = good_auth
                nav.require(session.closed and not session.source.blocks and
                            not (args.out / "denied.flac").exists(), "denied session retained output/cache")
                report["warm_cache_rejects_invalid_credentials"] = True
            finally:
                session.close()
            revised = SourceSession(account, song["id"])
            try:
                nav.acquire(account, song["id"], start, frames,
                            args.out / "before-revision.flac", session=revised)
                # Only the throwaway server's fixture copy is changed. Keep its
                # size and contents, advance mtime beyond Last-Modified resolution.
                file = state / "music/Stratum fixture.flac"
                stat = file.stat()
                os.utime(file, (stat.st_atime, stat.st_mtime + 2))
                # Navidrome 0.64.2 serves the scanned MediaFile.UpdatedAt as
                # Last-Modified, rather than stat'ing the file on each request.
                # Record that blind spot, then wait for its actual reported
                # revision to change after scanning before expecting rejection.
                observed = nav.RangeSource(account.url("stream", id=song["id"], format="raw", maxBitRate=0), song["size"])
                observed.revalidate()
                report["validator_unchanged_before_scan"] = observed.validator == revised.source.validator
                report["validator_before_scan"] = observed.validator
                account.request("startScan", fullScan="true")
                deadline = time.monotonic() + 20
                while observed.validator == revised.source.validator:
                    nav.require(time.monotonic() < deadline, "scan did not publish a new source validator")
                    time.sleep(0.1)
                    observed = nav.RangeSource(account.url("stream", id=song["id"], format="raw", maxBitRate=0), song["size"])
                    observed.revalidate()
                report["validator_after_scan"] = observed.validator
                try:
                    nav.acquire(account, song["id"], start, frames,
                                args.out / "changed.flac", session=revised)
                    raise AssertionError("warm cache accepted changed source revision")
                except RuntimeError as error:
                    nav.require("range" in str(error) or "validator" in str(error), "unexpected revision failure")
                nav.require(revised.closed and not revised.source.blocks and
                            not (args.out / "changed.flac").exists(), "revision failure retained output/cache")
                report["warm_cache_rejects_changed_last_modified_after_scan"] = True
            finally:
                revised.close()
    report["server_stopped"] = True
    (args.out / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(f"Exact cold/warm PCM; warm read {report['warm']['body_bytes_received']} byte; "
          "cached auth and revision rejection passed; server stopped.", flush=True)


if __name__ == "__main__":
    main()
