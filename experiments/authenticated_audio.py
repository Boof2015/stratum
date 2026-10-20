"""Additional real encoded-media checks for access_proof.py --audio.

Only uses that proof's disposable accounts and generated WAV fixtures. Silent.
"""
import hashlib
import json
import struct
import time
import urllib.error
import urllib.request

from navidrome import HTTP, require, run


def request(base, path, method="GET", body=None, token=None, opener=None):
    data = None if body is None else json.dumps(body).encode()
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = "Bearer " + token
    req = urllib.request.Request(base + path, data=data, method=method, headers=headers)
    try:
        response = (opener or HTTP).open(req, timeout=12)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        payload = response.read(4 * 1024 * 1024 + 65541)
        require(len(payload) <= 4 * 1024 * 1024 + 65540, "audio response exceeded limit")
        return response.status, dict(response.headers), payload


class AudioProbe:
    def __init__(self, base, tokens, tracks, sources, out, check, opener=None):
        self.opener = opener
        self.base, self.tokens, self.tracks = base, dict(tokens), tracks
        self.sources, self.out, self.check = sources, out / "audio", check
        self.out.mkdir()
        self.handles = {}
        self.metrics = {}
        for user in ("alice", "bob"):
            self.reopen(user, self.tokens[user])
        self.check("encoded handle cannot cross users", self.segment("bob", handle=self.handles["alice"])[0] == 404)
        self.check("denied track cannot be opened for encoding", self.request(base, "/audio/v1/playbacks/" + tracks["alice"], "POST", token=tokens["bob"])[0] == 404)
        for user in ("alice", "bob"):
            # Both fixtures are 48 kHz stereo PCM WAV. Independent real-engine
            # integration tests cover native 44.1 and 96 kHz as well.
            source = sources[user]
            reference = run(["ffmpeg", "-v", "error", "-nostdin", "-i", str(source), "-map", "0:a:0", "-f", "s32le", "pipe:1"])
            segments = []
            for i, quality in enumerate(("lossless", "opus-64", "opus-128")):
                start = i * 96000
                result = self.segment(user, start=start, quality=quality)
                metadata, audio = self.unpack(result)
                path = self.out / f"{user}-{i}.{('flac' if quality == 'lossless' else 'opus')}"
                path.write_bytes(audio)
                pcm = run(["ffmpeg", "-v", "error", "-nostdin", "-i", str(path), "-map", "0:a:0", "-f", "s32le", "pipe:1"])
                self.check(f"{user} {quality} decoder frame count matches contract", len(pcm) == metadata["decoded_frames"] * 8)
                first = metadata["discard_start_frames"] * 8
                presented = pcm[first:first + 96000 * 8]
                self.check(f"{user} {quality} native interval exact", len(presented) == 96000 * 8 and metadata["presentation"] == {"start": start, "frames": 96000})
                if quality == "lossless":
                    self.check(f"{user} lossless PCM equals independent source decode", presented == reference[start * 8:(start+96000)*8])
                segments.append({"start": start, "frames": 96000, "quality": quality, "payload_sha256": metadata["payload_sha256"]})
            self.metrics[user]["segments"] = segments
            warm, _ = self.unpack(self.segment(user))
            self.check(f"{user} actual encoded cache hit", warm["cache_hit"] is True)

        # Replace original PCM bytes while size and scanned metadata stay unchanged.
        # An existing handle must keep its frozen copy, while a fresh handle sees
        # new bytes. This does NOT prove atomicity during the initial HTTP response.
        source = sources["alice"]
        original = source.read_bytes()
        original_hash = hashlib.sha256(original).hexdigest()
        before = self.unpack(self.segment("alice", start=144000, quality="lossless"))[1]
        mutated = bytearray(original)
        for offset in range(44, len(mutated), 2):
            value = struct.unpack_from("<h", mutated, offset)[0]
            struct.pack_into("<h", mutated, offset, -value if value != -32768 else 32767)
        try:
            source.write_bytes(mutated)
            old_metadata, after = self.unpack(self.segment("alice", start=144000, quality="lossless"))
            self.check("same-size upstream mutation cannot change open playback PCM", before == after and old_metadata["source_sha256"] == original_hash)
            fresh = self.open("alice")
            self.check("fresh playback acquires changed original", fresh["source"]["sha256"] == hashlib.sha256(mutated).hexdigest() and fresh["source"]["sha256"] != original_hash)
            self.request(base, "/audio/v1/playback/" + fresh["playback"], "DELETE", token=tokens["alice"])
        finally:
            source.write_bytes(original)
        self.unpack(self.segment("alice"))  # Rewarm the exact interval used by revocation checks.

    def request(self, *args, **kwargs):
        return request(*args, **kwargs, opener=self.opener)

    def open(self, user):
        status, _, body = self.request(self.base, "/audio/v1/playbacks/" + self.tracks[user], "POST", token=self.tokens[user])
        require(status == 201, f"audio source preparation failed (HTTP {status}): {body[:100]!r}")
        return json.loads(body)

    def reopen(self, user, token):
        self.tokens[user] = token
        started = time.monotonic()
        opened = self.open(user)
        self.handles[user] = opened["playback"]
        self.check(user + " complete source hash matches original", opened["source"]["sha256"] == hashlib.sha256(self.sources[user].read_bytes()).hexdigest())
        self.unpack(self.segment(user))
        self.metrics.setdefault(user, {**{key: opened[key] for key in ("source", "sourcePolicy", "acquiredBytes", "acquisitionMs", "readyMs")},
                                       "firstSegmentMs": (time.monotonic() - started) * 1000.0,
                                       "firstSegmentQuality": "opus-128"})

    def segment(self, user, handle=None, start=192000, frames=96000, quality="opus-128"):
        return self.request(self.base, "/audio/v1/playback/" + (handle or self.handles[user]) + "/segment", "POST",
                       {"start": start, "frames": frames, "quality": quality}, self.tokens[user])

    def unpack(self, response):
        status, _, body = response
        require(status == 200, f"encoded delivery failed (HTTP {status})")
        length = struct.unpack_from(">I", body)[0]
        require(0 < length <= 65536 and 4+length < len(body), "invalid encoded envelope")
        metadata = json.loads(body[4:4+length])
        audio = body[4+length:]
        require(len(audio) == metadata["payload_bytes"] and hashlib.sha256(audio).hexdigest() == metadata["payload_sha256"], "encoded payload identity mismatch")
        return metadata, audio

    def denied(self, user, status, label):
        self.check(label + " blocks cached encoded audio", self.segment(user)[0] == status)

    def finish(self):
        (self.out / "report.json").write_text(json.dumps({"metrics": self.metrics,
            "limits": "Full-file acquisition baseline, local loopback and generated 60-second WAVs; no WAN or concurrent-write snapshot guarantee. No playback device."}, indent=2) + "\n")
