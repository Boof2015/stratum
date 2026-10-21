use crate::{
    process::{ffmpeg, run},
    timeline::{converted_frames, Interval, RATE, SOURCE_RATES},
};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

mod preparation;
pub(crate) use preparation::{FileStamp, PreparedEncoder};

pub const EXPERIMENT: &str = "stratum-audio-poc-2";
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Quality {
    Lossless,
    #[serde(rename = "opus-64")]
    Opus64,
    #[serde(rename = "opus-128")]
    Opus128,
}

impl Quality {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "lossless" => Ok(Self::Lossless),
            "opus-64" => Ok(Self::Opus64),
            "opus-128" => Ok(Self::Opus128),
            _ => bail!("quality must be lossless, opus-64, or opus-128"),
        }
    }
    pub fn extension(self) -> &'static str {
        if self == Self::Lossless {
            "flac"
        } else {
            "opus"
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceInfo {
    pub sha256: String,
    pub codec: String,
    pub sample_rate: u64,
    pub channels: u32,
    pub bits_per_sample: u32,
    pub frames: u64,
    pub bytes: u64,
}

pub struct Source {
    path: PathBuf,
    stamp: FileStamp,
    pub info: SourceInfo,
}

#[derive(Deserialize)]
struct Probe {
    streams: Vec<Stream>,
}
#[derive(Deserialize)]
struct Stream {
    codec_name: String,
    sample_rate: String,
    channels: u32,
    #[serde(default)]
    bits_per_raw_sample: String,
    #[serde(default)]
    bits_per_sample: u32,
    duration_ts: u64,
    time_base: String,
}

impl Source {
    pub fn open(path: &Path) -> Result<Self> {
        let path = path.canonicalize().context("could not open source file")?;
        let metadata = fs::metadata(&path)?;
        ensure!(metadata.is_file(), "source must be a regular local file");
        let stamp = FileStamp::read(&path)?;
        let bytes = run(
            Command::new("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-protocol_whitelist",
                    "file,pipe",
                    "-select_streams",
                    "a:0",
                    "-show_streams",
                    "-of",
                    "json",
                ])
                .arg(&path),
            TIMEOUT,
            1_048_576,
        )?;
        let probe: Probe =
            serde_json::from_slice(&bytes).context("source has no exact audio duration")?;
        let stream = probe
            .streams
            .first()
            .context("source has no audio stream")?;
        let rate: u64 = stream.sample_rate.parse()?;
        ensure!(
            SOURCE_RATES.contains(&rate),
            "this proof supports 44.1/48/96 kHz audio; got {rate} Hz (no silent resampling)"
        );
        ensure!(
            (1..=2).contains(&stream.channels),
            "this first proof supports mono or stereo"
        );
        ensure!(["flac", "pcm_s16le", "pcm_s24le"].contains(&stream.codec_name.as_str()),
            "this first proof accepts FLAC or 16/24-bit PCM WAV; lossy originals are a later experiment");
        let bits = stream
            .bits_per_raw_sample
            .parse()
            .unwrap_or(stream.bits_per_sample);
        ensure!(
            [16, 24].contains(&bits),
            "this proof supports 16/24-bit source PCM, got {bits}"
        );
        let (num, den) = stream
            .time_base
            .split_once('/')
            .context("invalid source time base")?;
        let numerator = u128::from(stream.duration_ts) * num.parse::<u128>()? * u128::from(rate);
        let denominator = den.parse::<u128>()?;
        ensure!(
            denominator > 0 && numerator % denominator == 0,
            "source duration is not an exact frame count"
        );
        let frames = u64::try_from(numerator / denominator)?;
        ensure!(
            frames >= 16,
            "this proof requires at least 16 source frames (the minimum FLAC block size)"
        );
        let sha256 = hash_file(&path)?;
        stamp.check(&path)?;
        Ok(Self {
            path,
            stamp,
            info: SourceInfo {
                sha256,
                codec: stream.codec_name.clone(),
                sample_rate: rate,
                channels: stream.channels,
                bits_per_sample: bits,
                frames,
                bytes: metadata.len(),
            },
        })
    }

    fn check_unchanged(&self) -> Result<()> {
        self.stamp.check(&self.path)
    }

    /// Only the service's immutable, private staging file can skip a fresh probe
    /// and full hash in subsequent workers. Mutable/local proof inputs cannot.
    pub(crate) fn preparation_stamp(&self) -> Result<FileStamp> {
        self.check_unchanged()?;
        ensure!(self.stamp.readonly(), "prepared source must be read-only");
        Ok(self.stamp.clone())
    }

    pub(crate) fn resume(path: &Path, info: SourceInfo, stamp: FileStamp) -> Result<Self> {
        let path = path.canonicalize()?;
        ensure!(stamp.readonly(), "prepared source must be read-only");
        stamp.check(&path)?;
        ensure!(
            fs::metadata(&path)?.len() == info.bytes,
            "prepared source size changed"
        );
        Ok(Self { path, info, stamp })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// All trim counts refer to PCM AFTER the container decoder has applied Opus
/// pre-skip and end trimming. Applying pre-skip twice would lose samples.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentMetadata {
    pub experiment: String,
    pub source_sha256: String,
    pub encoder: String,
    pub quality: Quality,
    pub sample_rate: u64,
    /// Actual payload decoder clock; Opus always uses 48 kHz in this proof.
    pub codec_sample_rate: u64,
    /// Frames after codec pre-skip/end trimming, before client rate conversion.
    pub codec_decoded_frames: u64,
    /// Explicit extra native frame from rounding a non-integral round trip.
    pub resample_tail_frames: u64,
    pub channels: u32,
    pub presentation: Interval,
    pub encoded_interval: Interval,
    pub discard_start_frames: u64,
    pub discard_end_frames: u64,
    pub decoded_frames: u64,
    pub codec_preskip_frames: u16,
    pub source_bytes_preserved: bool,
    pub source_pcm_preserved: bool,
    pub payload_sha256: String,
    pub payload_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct Segment {
    pub path: PathBuf,
    pub metadata: SegmentMetadata,
    pub cache_hit: bool,
    pub prepare_ms: f64,
}

pub struct Engine {
    pub source: Source,
    cache: PathBuf,
    encoder: PreparedEncoder,
}

impl Engine {
    pub fn new(source: Source, cache: &Path) -> Result<Self> {
        Self::prepared(source, cache, PreparedEncoder::capture()?)
    }

    pub(crate) fn prepared(source: Source, cache: &Path, encoder: PreparedEncoder) -> Result<Self> {
        source.check_unchanged()?;
        encoder.check()?;
        fs::create_dir_all(cache)?;
        Ok(Self {
            source,
            cache: cache.canonicalize()?,
            encoder,
        })
    }

    pub fn segment(&self, start: u64, frames: u64, quality: Quality) -> Result<Segment> {
        let began = Instant::now();
        self.source.check_unchanged()?;
        self.encoder.check()?;
        let rate = self.source.info.sample_rate;
        let presentation = Interval::at_rate(start, frames, self.source.info.frames, rate)?;
        // FLAC cannot encode a block shorter than 16 frames. A tiny requested
        // tail includes real neighboring source audio, then the client trims it.
        let encoded = if quality == Quality::Lossless && frames >= 16 {
            presentation
        } else {
            presentation.context_at_rate(self.source.info.frames, rate)
        };
        let codec_rate = if quality == Quality::Lossless {
            rate
        } else {
            RATE
        };
        // Downsampling 96 kHz needs twice as many native frames for the
        // same filter window. Reject too-short whole sources instead of padding.
        let minimum_context = if rate == 96_000 { 64 } else { 32 };
        ensure!(
            codec_rate == rate || encoded.frames >= minimum_context,
            "rate-converted Opus needs at least {minimum_context} real source frames; no silence padding is added"
        );
        let codec_frames = converted_frames(encoded.frames, rate, codec_rate)?;
        let decoded_frames = converted_frames(codec_frames, codec_rate, rate)?;
        let resample_tail = decoded_frames - encoded.frames;
        let key = hash_bytes(&serde_json::to_vec(&(
            EXPERIMENT,
            &self.source.info.sha256,
            &self.encoder.identity,
            quality,
            presentation,
            encoded,
        ))?);
        let path = self.cache.join(format!("{key}.{}", quality.extension()));
        let manifest = self.cache.join(format!("{key}.json"));
        let mut metadata = SegmentMetadata {
            experiment: EXPERIMENT.into(),
            source_sha256: self.source.info.sha256.clone(),
            encoder: self.encoder.identity.clone(),
            quality,
            sample_rate: rate,
            codec_sample_rate: codec_rate,
            codec_decoded_frames: codec_frames,
            resample_tail_frames: resample_tail,
            channels: self.source.info.channels,
            presentation,
            encoded_interval: encoded,
            discard_start_frames: start - encoded.start,
            discard_end_frames: encoded.end() - presentation.end() + resample_tail,
            decoded_frames,
            codec_preskip_frames: 0,
            source_bytes_preserved: false,
            source_pcm_preserved: quality == Quality::Lossless,
            payload_sha256: String::new(),
            payload_bytes: 0,
        };
        if let Ok(bytes) = fs::read(&manifest) {
            if let Ok(saved) = serde_json::from_slice::<SegmentMetadata>(&bytes) {
                // Validate cache metadata as well as bytes; damaged entries regenerate.
                metadata.payload_sha256 = saved.payload_sha256.clone();
                metadata.payload_bytes = saved.payload_bytes;
                metadata.codec_preskip_frames = saved.codec_preskip_frames;
                // FFmpeg's resampler can flush either N or N+1 native frames
                // after the fractional-rate codec round trip. Accept only the
                // measured tail within that one-frame bound; never a short N.
                let valid_tail = saved.resample_tail_frames <= resample_tail
                    && saved.decoded_frames == encoded.frames + saved.resample_tail_frames
                    && saved.discard_end_frames
                        == encoded.end() - presentation.end() + saved.resample_tail_frames;
                if valid_tail {
                    metadata.resample_tail_frames = saved.resample_tail_frames;
                    metadata.decoded_frames = saved.decoded_frames;
                    metadata.discard_end_frames = saved.discard_end_frames;
                }
                if valid_tail
                    && saved == metadata
                    && fs::metadata(&path).map(|m| m.len()).ok() == Some(saved.payload_bytes)
                    && hash_file(&path).ok().as_ref() == Some(&saved.payload_sha256)
                {
                    return Ok(Segment {
                        path,
                        metadata: saved,
                        cache_hit: true,
                        prepare_ms: began.elapsed().as_secs_f64() * 1000.0,
                    });
                }
            }
        }
        let staging = tempfile::tempdir_in(&self.cache)?;
        let output = staging
            .path()
            .join(format!("audio.{}", quality.extension()));
        // Seek to a whole second, then trim by integer frames. This avoids rounding
        // a native-frame boundary into FFmpeg's microsecond -ss argument.
        let seek_seconds = encoded.start / rate;
        let skip = encoded.start - seek_seconds * rate;
        let mut filter = format!(
            "atrim=start_sample={skip}:end_sample={},asetpts=PTS-STARTPTS",
            skip + encoded.frames
        );
        if rate != codec_rate {
            filter.push_str(&format!(
                ",aresample={codec_rate}:resampler=swr:exact_rational=1:async=0:dither_method=none"
            ));
        }
        let mut command = self.encoder.command();
        command
            .args([
                "-protocol_whitelist",
                "file,pipe",
                "-ss",
                &seek_seconds.to_string(),
                "-i",
            ])
            .arg(self.source.path())
            .args([
                "-map",
                "0:a:0",
                "-vn",
                "-sn",
                "-dn",
                "-map_metadata",
                "-1",
                "-af",
            ])
            .arg(filter)
            .args([
                "-ar",
                &codec_rate.to_string(),
                "-ac",
                &self.source.info.channels.to_string(),
                "-threads",
                "1",
            ]);
        match quality {
            Quality::Lossless => {
                command.args([
                    "-c:a",
                    "flac",
                    "-frame_size",
                    "4096",
                    "-sample_fmt",
                    "s32",
                    "-bits_per_raw_sample",
                    &self.source.info.bits_per_sample.to_string(),
                    "-compression_level",
                    "5",
                ]);
            }
            Quality::Opus64 | Quality::Opus128 => {
                command.args([
                    "-c:a",
                    "libopus",
                    "-b:a",
                    if quality == Quality::Opus64 {
                        "64k"
                    } else {
                        "128k"
                    },
                    "-vbr",
                    "off",
                    "-application",
                    "audio",
                    "-frame_duration",
                    "20",
                    "-apply_phase_inv",
                    "0",
                ]);
            }
        }
        run(command.arg(&output), TIMEOUT, 1_048_576)?;
        self.source.check_unchanged()?;
        let decoded = decode_between_using(&output, self.source.info.channels, codec_frames, codec_frames, None, self.encoder.command())
            .with_context(|| format!("codec frames: rate={rate}, start={start}, frames={frames}, encoded={encoded:?}, quality={quality:?}"))?;
        ensure!(
            decoded.len() as u64 == codec_frames * u64::from(self.source.info.channels),
            "encoder/decoder returned the wrong frame count"
        );
        if rate != codec_rate {
            let native = decode_between_using(
                &output,
                self.source.info.channels,
                encoded.frames,
                decoded_frames,
                Some(rate),
                self.encoder.command(),
            )
            .context("client rate conversion count")?;
            metadata.decoded_frames = native.len() as u64 / u64::from(self.source.info.channels);
            metadata.resample_tail_frames = metadata.decoded_frames - encoded.frames;
            metadata.discard_end_frames =
                encoded.end() - presentation.end() + metadata.resample_tail_frames;
        }
        self.source.check_unchanged()?;
        self.encoder.check()?;
        let payload = fs::read(&output)?;
        metadata.codec_preskip_frames = if quality == Quality::Lossless {
            0
        } else {
            opus_preskip(&payload)?
        };
        metadata.payload_sha256 = hash_bytes(&payload);
        metadata.payload_bytes = payload.len() as u64;
        let temp_manifest = staging.path().join("metadata.json");
        fs::write(&temp_manifest, serde_json::to_vec_pretty(&metadata)?)?;
        // Publish completed payload first, then the manifest; never cache partial work.
        fs::rename(output, &path)?;
        fs::rename(temp_manifest, &manifest)?;
        Ok(Segment {
            path,
            metadata,
            cache_hit: false,
            prepare_ms: began.elapsed().as_secs_f64() * 1000.0,
        })
    }
}

pub fn decode(path: &Path, channels: u32, expected_frames: u64) -> Result<Vec<i32>> {
    decode_at_rate(path, channels, expected_frames, None)
}

pub fn decode_at_rate(
    path: &Path,
    channels: u32,
    expected_frames: u64,
    rate: Option<u64>,
) -> Result<Vec<i32>> {
    decode_between(path, channels, expected_frames, expected_frames, rate)
}

fn decode_between(
    path: &Path,
    channels: u32,
    minimum_frames: u64,
    maximum_frames: u64,
    rate: Option<u64>,
) -> Result<Vec<i32>> {
    decode_between_using(
        path,
        channels,
        minimum_frames,
        maximum_frames,
        rate,
        ffmpeg(),
    )
}

fn decode_between_using(
    path: &Path,
    channels: u32,
    minimum_frames: u64,
    maximum_frames: u64,
    rate: Option<u64>,
    mut command: Command,
) -> Result<Vec<i32>> {
    let minimum_bytes = minimum_frames
        .checked_mul(u64::from(channels))
        .and_then(|n| n.checked_mul(4))
        .context("PCM size overflow")?;
    let maximum_bytes = maximum_frames
        .checked_mul(u64::from(channels))
        .and_then(|n| n.checked_mul(4))
        .context("PCM size overflow")?;
    command
        .args(["-protocol_whitelist", "file,pipe", "-i"])
        .arg(path);
    if let Some(rate) = rate {
        // No async clock correction, padding, crossfade or dithering.
        command.args([
            "-af",
            &format!("aresample={rate}:resampler=swr:exact_rational=1:async=0:dither_method=none"),
        ]);
    }
    let bytes = run(
        command.args([
            "-map",
            "0:a:0",
            "-vn",
            "-f",
            "s32le",
            "-c:a",
            "pcm_s32le",
            "pipe:1",
        ]),
        TIMEOUT,
        maximum_bytes + 65536,
    )?;
    ensure!(
        (minimum_bytes..=maximum_bytes).contains(&(bytes.len() as u64))
            && (bytes.len() as u64).is_multiple_of(u64::from(channels) * 4),
        "decoded {} bytes, expected {minimum_bytes}..={maximum_bytes}",
        bytes.len()
    );
    Ok(bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| i32::from_le_bytes(*c))
        .collect())
}

pub fn presentation_pcm(segment: &Segment) -> Result<Vec<i32>> {
    ensure!(
        hash_file(&segment.path)? == segment.metadata.payload_sha256,
        "segment payload changed"
    );
    let metadata = &segment.metadata;
    let pcm = decode_at_rate(
        &segment.path,
        metadata.channels,
        metadata.decoded_frames,
        (metadata.codec_sample_rate != metadata.sample_rate).then_some(metadata.sample_rate),
    )?;
    let start = usize::try_from(metadata.discard_start_frames * u64::from(metadata.channels))?;
    let count = usize::try_from(metadata.presentation.frames * u64::from(metadata.channels))?;
    Ok(pcm
        .get(start..start + count)
        .context("invalid presentation trims")?
        .to_vec())
}

fn opus_preskip(payload: &[u8]) -> Result<u16> {
    // Identification is the first packet on the first Ogg page.
    ensure!(
        payload.starts_with(b"OggS") && payload.len() >= 28,
        "invalid Ogg header"
    );
    let offset = 27 + usize::from(payload[26]);
    ensure!(
        payload.get(offset..offset + 8) == Some(b"OpusHead"),
        "missing Opus identification header"
    );
    let bytes = payload
        .get(offset + 10..offset + 12)
        .context("truncated Opus header")?;
    Ok(u16::from_le_bytes(bytes.try_into().unwrap()))
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
