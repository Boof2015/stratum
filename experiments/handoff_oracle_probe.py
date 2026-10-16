#!/usr/bin/env python3
"""Check the handoff oracle rejects corrupted copies of an all-lossless run.

No server or playback. Original captures/segments are never changed.
"""
import argparse
import copy
import json
from pathlib import Path
import shutil
import subprocess
import tempfile

import navidrome as nav


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--checker", type=Path, required=True)
    parser.add_argument("--delivery", type=Path, required=True)
    args = parser.parse_args()
    checker, source = args.checker.resolve(strict=True), args.delivery.resolve(strict=True)
    original = json.loads(source.read_text())
    nav.require(all(segment["quality"] == "lossless" for segment in original["segments"]), "use all-lossless run")
    captured = (source.parent / "rendered.s32le").read_bytes()
    nav.require(not original["underruns"], "use a run without starvation")
    boundary = original["tracks"][0]["frames"] * 8
    with tempfile.TemporaryDirectory(prefix="stratum-handoff-oracle-") as temporary:
        folder = Path(temporary)
        for segment in original["segments"]:
            shutil.copyfile(source.parent / segment["pcm"], folder / segment["pcm"])
        report = folder / "delivery.json"
        raw = folder / "rendered.s32le"

        def check(delivery, pcm):
            report.write_text(json.dumps(delivery))
            raw.write_bytes(pcm)
            return json.loads(nav.run([str(checker), str(report)]))

        good = check(original, captured)
        nav.require(all(good[key] for key in ("exact_frames", "lossless_exact", "rendered_matches", "zero_lag")),
                    "baseline capture does not pass")
        corruptions = {
            "silence_at_join": captured[:boundary] + bytes(8) + captured[boundary + 8:],
            "missing_last_frame": captured[:boundary - 8] + captured[boundary:] + bytes(8),
            "repeated_last_frame": captured[:boundary] + captured[boundary - 8:boundary] + captured[boundary:-8],
        }
        for name, pcm in corruptions.items():
            nav.require(not check(original, pcm)["rendered_matches"], "oracle missed " + name)

        # Corrupt BOTH the submitted segment and the capture. Self-comparison
        # would pass; the independent original-source reference must reject it.
        first_next = next(segment for segment in original["segments"] if segment["track"] == 1)
        segment_path = folder / first_next["pcm"]
        changed = bytearray(segment_path.read_bytes())
        changed[1] ^= 1
        segment_path.write_bytes(changed)
        changed_capture = bytearray(captured)
        changed_capture[boundary + 1] ^= 1
        result = check(original, changed_capture)
        nav.require(result["rendered_matches"] and not result["lossless_exact"], "independent PCM oracle missed corruption")
        shutil.copyfile(source.parent / first_next["pcm"], segment_path)

        wrong_track = copy.deepcopy(original)
        next(segment for segment in wrong_track["segments"] if segment["track"] == 1)["track"] = 0
        report.write_text(json.dumps(wrong_track))
        result = subprocess.run([str(checker), str(report)], capture_output=True, timeout=30)
        nav.require(result.returncode != 0 and b"cross-track interval" in result.stderr, "oracle missed wrong track")
        for index, message in ((0, b"actual EOF"), (1, b"frame zero")):
            shifted = copy.deepcopy(original)
            shifted["tracks"][index]["frames"] -= 1
            if index == 1:
                shifted["tracks"][index]["origin"] = 1
            report.write_text(json.dumps(shifted))
            result = subprocess.run([str(checker), str(report)], capture_output=True, timeout=30)
            nav.require(result.returncode != 0 and message in result.stderr, "oracle accepted wrong file edge")
    print("Handoff oracle: baseline and 7 corruption controls passed.", flush=True)


if __name__ == "__main__":
    main()
