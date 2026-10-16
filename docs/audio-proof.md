# Audio interval proof, revision 1

This is an experimental contract exercised by the local CLI, not Stratum v1's
final protocol. Changing it is expected as we test more formats and real delivery.

## Clock and requests

The first proof deliberately uses only native 48,000 Hz, mono/stereo, 16/24-bit
lossless sources. The clock unit is one PCM frame (one sample per channel).
Requests are half-open intervals `[start, start + frames)` with an integer start
and a positive count. No requested interval exceeds 30 seconds or the source end.
Invalid or short results fail; they are not padded with silence.

The source must contain at least 16 frames. It is fingerprinted with SHA-256 when
opened. Files are assumed immutable during an engine session; size/mtime changes
are detected before requests and after encoding. This is a local fixture identity,
not an authentication mechanism or the eventual remote revision strategy.

## Representations

| Request | Encoded payload | PCM treatment |
| --- | --- | --- |
| `lossless` | FLAC | Preserves the supported source PCM and bit depth |
| `opus-64` | Ogg Opus, 64 kbps CBR | Lossy, 48 kHz, same channels |
| `opus-128` | Ogg Opus, 128 kbps CBR | Lossy, 48 kHz, same channels |

Every payload contains a self-contained codec/container header. These are
experimental encoding choices, not a negotiated production capability set. Opus
uses 20 ms packets, audio mode, and intensity-stereo phase inversion disabled.

For Opus, the encoder receives up to 120 ms of real source audio before and after
the requested interval, clamped at track edges. Context is an experimental encoder
warm-up/tail choice. It is not the codec pre-skip and is not proof of inaudibility.
Lossless intervals normally need no extra context; fragments shorter than FLAC's
16-frame minimum block include neighboring real source audio and trim it afterward.

Source extraction first seeks to the preceding whole second, then uses integer
`atrim` sample positions. This avoids representing an odd frame boundary as a
rounded microsecond seek. It can decode up to another second before the desired
context. It does not establish how many compressed bytes a remote source needs.

## Decode and present

For a segment's metadata:

1. Decode the complete self-contained payload. The container decoder applies
   Opus pre-skip and final granule trimming. `codec_preskip_frames` is diagnostic;
   **do not discard that number again**.
2. Verify the decoded frame count equals `decoded_frames`.
3. Discard `discard_start_frames` of decoded PCM and any trailing
   `discard_end_frames`.
4. Present exactly `presentation.frames` beginning at `presentation.start` on the
   source timeline. The next segment starts at the previous presentation end.

The accounting invariant is:

```text
discard_start_frames + presentation.frames + discard_end_frames
    == decoded_frames == encoded_interval.frames
```

For example, requesting `[96001, 192002)` at Opus quality from a sufficiently long
source encodes `[90241, 197762)`. The decoder returns 107,521 frames after codec
trimming. Discarding 5,760 frames at each end leaves exactly 96,001 presentation
frames. The reported codec pre-skip (312 frames with the tested build) is already
accounted for by decoding and is not part of these two context discards.

No overlap blend, crossfade, silence pad or lag correction is applied in this proof.
The sample clock provides continuity of position; lossy reconstruction can still
change waveform shape at a switch. Boundary step differences are reported so they
can be investigated rather than hidden by a timing assertion.

## Cache and resource behavior

The key includes source SHA-256, experiment revision, FFmpeg build/configuration,
quality and both intervals. Completed payloads and metadata are published from a
temporary directory. Cache hits verify metadata and the payload hash. Missing or
damaged entries regenerate. Source edits require reopening the source.

The proof has no eviction or concurrent encoding deduplication. It is a local,
single-client experiment. Cache files convey no permission to another user; a
future serving layer must authorize both cold requests and cache hits.

FFmpeg and ffprobe run directly with explicit argument arrays and local-file
protocol restrictions. Each invocation is bounded to 30 seconds and its expected
stdout size. stdout/stderr go through temporary files to avoid pipe deadlocks.
The engine validates decoded frame counts even on a cold encode; this verification
cost is included in `prepare_ms` and may not belong on a future production path.

## Measurement interpretation

The demo performs a separate, complete source decode as an oracle. Lossless
sections must match it exactly. Each segment is checked in beginning/middle/end
windows, per channel, for the best correlation within ±32 frames. An injected
one-frame shift test demonstrates that the check detects that error. Silence and
periodic content can make the estimate inconclusive or ambiguous; correlation
cannot by itself prove an audible transition is clean.

`alignment_windows_at_zero_lag` describes evaluable windows; inspect
`alignment_inconclusive_windows` too. The default synthetic fixture must have no
inconclusive windows and pass all alignment checks. The demo records real music
results without pretending a correlation estimate is a universal pass/fail rule.

Preparation time measures local encoding, decoder validation and cache publication
or lookup. Source probing/hashing and the client's later decode are outside that
timer. A warm cache reading fast on one machine is not a network streaming result.
`payload_bytes` includes codec context and container overhead.

## References

- [Ogg Opus: pre-skip, PCM positions, end trimming and seeking](https://www.rfc-editor.org/rfc/rfc7845.html#section-4).
- [FFmpeg input seeking](https://ffmpeg.org/ffmpeg.html#Main-options).
- [FFmpeg sample-based audio trimming](https://ffmpeg.org/ffmpeg-filters.html#atrim).

These describe codec/tool behavior. Stratum's context size, supported formats and
sample-domain contract above are experimental choices tested by this repository.

