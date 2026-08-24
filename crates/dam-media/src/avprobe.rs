//! Audio and video metadata, via ffprobe.
//!
//! Fills `assets.duration_ms` and, for video, the dimensions the grid lays out from. Video *derivatives*
//! are M3.5; this is the metadata half, which ingest needs immediately — an audio file with no duration
//! cannot be listed, let alone played.
//!
//! Like libvips, ffmpeg runs inside [`crate::sandbox`]. Its demuxers face the same hostile input and have
//! their own CVE history, and the same reasoning applies: a malformed container should cost a bounded
//! subprocess, not the worker.
//!
//! ## ffprobe calls a PNG a video
//!
//! The detail that shapes this module. A still image is described as a stream of `codec_type: "video"`,
//! with `format_name: "png_pipe"` and **no duration at all**. Treating "has a video stream" as "is a
//! video" would route every photograph into the video pipeline, so [`AvProbe::is_timed`] is derived from
//! the presence of a duration rather than from the stream type.

use crate::sandbox::{Limits, Outcome, Sandbox};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("ffmpeg is not available: {0}")]
    Unavailable(String),

    #[error("ffprobe could not read {path}: {detail}")]
    Rejected { path: String, detail: String },

    #[error("ffprobe exceeded its limits reading {path}: {outcome}")]
    Bounded { path: String, outcome: String },

    #[error("ffprobe returned output that is not the JSON it was asked for: {0}")]
    Unparseable(String),

    #[error(transparent)]
    Sandbox(#[from] crate::sandbox::Error),
}

type Result<T> = std::result::Result<T, Error>;

/// Absolute paths to the ffmpeg binaries.
///
/// Absolute for the same reason as the vips toolchain: the sandbox clears `PATH`, so the lookup has to
/// happen in the parent — and pointing a decoder at untrusted bytes is no place for `PATH` ambiguity
/// about which binary ran.
#[derive(Debug, Clone)]
pub struct AvToolchain {
    ffprobe: PathBuf,
    ffmpeg: PathBuf,
}

impl AvToolchain {
    pub fn discover() -> Result<Self> {
        let dir = if let Ok(dir) = std::env::var("DAMRS_FFMPEG_BIN") {
            PathBuf::from(dir)
        } else {
            which("ffprobe")
                .and_then(|p| p.parent().map(Path::to_path_buf))
                .ok_or_else(|| {
                    Error::Unavailable(
                        "no `ffprobe` on PATH and DAMRS_FFMPEG_BIN is unset; it is pinned in \
                         .mise.toml, so `mise install` should provide it"
                            .to_owned(),
                    )
                })?
        };

        let ffprobe = dir.join("ffprobe");
        let ffmpeg = dir.join("ffmpeg");
        for binary in [&ffprobe, &ffmpeg] {
            if !binary.exists() {
                return Err(Error::Unavailable(format!(
                    "{} does not exist",
                    binary.display()
                )));
            }
        }
        Ok(Self {
            ffprobe: ffprobe.canonicalize().unwrap_or(ffprobe),
            ffmpeg: ffmpeg.canonicalize().unwrap_or(ffmpeg),
        })
    }

    pub fn ffprobe(&self) -> &Path {
        &self.ffprobe
    }

    pub fn ffmpeg(&self) -> &Path {
        &self.ffmpeg
    }
}

fn which(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(binary))
        .find(|candidate| candidate.is_file())
}

/// What ffprobe reports.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AvProbe {
    /// Container duration in milliseconds. `None` for a still image.
    pub duration_ms: Option<i64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub channels: Option<u32>,
    pub sample_rate: Option<u32>,
    pub bit_rate: Option<i64>,
    /// Frames per second, from the `r_frame_rate` rational.
    pub frame_rate: Option<f64>,
    /// Whether this is time-based media at all.
    ///
    /// Derived from the duration, **not** from the stream type: ffprobe labels a PNG's single frame as a
    /// `video` stream, so the stream type would put every photograph in the video pipeline.
    pub is_timed: bool,
    /// ffprobe's container name, e.g. `wav`, `mov,mp4,m4a,3gp,3g2,mj2`, `png_pipe`.
    pub format_name: Option<String>,
}

/// Probes with the default limits.
pub async fn probe(tools: &AvToolchain, path: &Path) -> Result<AvProbe> {
    probe_with_limits(tools, path, Limits::default()).await
}

/// Probes, bounded by `limits`.
pub async fn probe_with_limits(
    tools: &AvToolchain,
    path: &Path,
    limits: Limits,
) -> Result<AvProbe> {
    let sandbox = Sandbox::new(limits)?;
    let outcome = sandbox
        .run(
            &tools.ffprobe.to_string_lossy(),
            &[
                // `-v quiet` so stdout is JSON and nothing else; the diagnosis on failure comes from the
                // exit status and stderr, which the sandbox captures separately.
                "-v",
                "error",
                "-print_format",
                "json",
                "-show_format",
                "-show_streams",
                &path.to_string_lossy(),
            ],
        )
        .await?;

    match &outcome {
        Outcome::Ok { stdout, .. } => parse(&String::from_utf8_lossy(stdout)),
        Outcome::Failed { stderr, .. } => Err(Error::Rejected {
            path: path.display().to_string(),
            detail: String::from_utf8_lossy(stderr).trim().to_owned(),
        }),
        Outcome::Killed { .. } | Outcome::TimedOut { .. } => Err(Error::Bounded {
            path: path.display().to_string(),
            outcome: format!("{outcome:?}"),
        }),
    }
}

/// Runs ffmpeg with the default limits. For generating test fixtures and short operations.
///
/// **Not for transcoding.** The default wall clock is 120 seconds and there is a CPU cap, both of which are
/// right for a probe and fatal for real media — see `video::limits_for`.
pub async fn run_ffmpeg(tools: &AvToolchain, args: &[&str]) -> Result<()> {
    run_ffmpeg_with_limits(tools, args, Limits::default()).await
}

/// Runs ffmpeg under explicit limits, discarding its output.
pub async fn run_ffmpeg_with_limits(
    tools: &AvToolchain,
    args: &[&str],
    limits: Limits,
) -> Result<()> {
    run_ffmpeg_capturing(tools, args, limits).await.map(|_| ())
}

/// Runs ffmpeg under explicit limits and returns its **stderr**.
///
/// stderr, not stdout, and that is not an oversight: ffmpeg writes its banner, stream information, progress
/// and — critically — filter output like `loudnorm`'s measurement there. stdout is reserved for media, which
/// is why `-f null -` writes nothing to it. A caller reading stdout for a measurement gets an empty string
/// and concludes the file has no audio.
pub async fn run_ffmpeg_capturing(
    tools: &AvToolchain,
    args: &[&str],
    limits: Limits,
) -> Result<String> {
    let outcome = Sandbox::new(limits)?
        .run(&tools.ffmpeg.to_string_lossy(), args)
        .await?;
    match &outcome {
        Outcome::Ok { stderr, .. } => Ok(String::from_utf8_lossy(stderr).into_owned()),
        Outcome::Failed { stderr, .. } => Err(Error::Rejected {
            path: args.join(" "),
            detail: String::from_utf8_lossy(stderr).trim().to_owned(),
        }),
        other => Err(Error::Bounded {
            path: args.join(" "),
            outcome: format!("{other:?}"),
        }),
    }
}

/// Parses ffprobe's JSON.
///
/// Every numeric field is read leniently, because ffprobe is inconsistent about types by design: `channels`
/// arrives as a number while `sample_rate` and `duration` arrive as strings. Assuming either shape breaks
/// on half the formats.
fn parse(json: &str) -> Result<AvProbe> {
    let root: serde_json::Value =
        serde_json::from_str(json).map_err(|e| Error::Unparseable(format!("{e}: {json}")))?;

    let format = root.get("format");
    let duration_ms = format
        .and_then(|f| f.get("duration"))
        .and_then(number)
        // Rounded, not truncated: "1.500000" is 1500 ms, and truncating to whole seconds loses half a
        // second on every clip — the difference between a correct HLS manifest and a player that stalls.
        .map(|seconds| (seconds * 1000.0).round() as i64)
        .filter(|ms| *ms > 0);

    let mut probe = AvProbe {
        duration_ms,
        // A duration is what makes something time-based. See the module docs: ffprobe calls a PNG a video.
        is_timed: duration_ms.is_some(),
        bit_rate: format
            .and_then(|f| f.get("bit_rate"))
            .and_then(number)
            .map(|v| v as i64),
        format_name: format
            .and_then(|f| f.get("format_name"))
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        ..AvProbe::default()
    };

    for stream in root
        .get("streams")
        .and_then(|s| s.as_array())
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let kind = stream.get("codec_type").and_then(|v| v.as_str());
        let codec = stream
            .get("codec_name")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        match kind {
            // First stream of each kind wins, and the guard rather than an inner `if` is what
            // `clippy::collapsible_match` asks for: a later stream of a kind already recorded falls
            // through to `_` and is ignored, which is the same answer the nested form gave. A file with
            // several video streams is a mezzanine or a mistake, and taking the first matches what a
            // player shows.
            Some("video") if probe.video_codec.is_none() => {
                probe.video_codec = codec;
                probe.width = stream.get("width").and_then(number).map(|v| v as u32);
                probe.height = stream.get("height").and_then(number).map(|v| v as u32);
                probe.frame_rate = stream.get("r_frame_rate").and_then(rational);
            }
            Some("audio") if probe.audio_codec.is_none() => {
                probe.audio_codec = codec;
                probe.channels = stream.get("channels").and_then(number).map(|v| v as u32);
                probe.sample_rate = stream.get("sample_rate").and_then(number).map(|v| v as u32);
            }
            _ => {}
        }
    }

    if probe.width.is_none() && probe.audio_codec.is_none() && probe.duration_ms.is_none() {
        return Err(Error::Unparseable(format!(
            "ffprobe found neither dimensions, audio, nor a duration: {json}"
        )));
    }
    Ok(probe)
}

/// A number that may be encoded as a JSON number or a string.
fn number(value: &serde_json::Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}

/// A rational like `"10/1"` or `"30000/1001"`.
///
/// `r_frame_rate` is always a fraction, and 30000/1001 — NTSC's 29.97 — is why it cannot be read as a
/// plain number.
fn rational(value: &serde_json::Value) -> Option<f64> {
    let text = value.as_str()?;
    let (numerator, denominator) = text.split_once('/')?;
    let numerator: f64 = numerator.parse().ok()?;
    let denominator: f64 = denominator.parse().ok()?;
    (denominator != 0.0).then(|| numerator / denominator)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_string_encoded_number_parses_the_same_as_a_json_number() {
        // ffprobe is inconsistent by design: `channels` is a number, `sample_rate` a string. Handling one
        // shape breaks half the formats.
        assert_eq!(number(&serde_json::json!(44100)), Some(44100.0));
        assert_eq!(number(&serde_json::json!("44100")), Some(44100.0));
        assert_eq!(number(&serde_json::json!("not a number")), None);
    }

    #[test]
    fn ntsc_frame_rates_survive_the_rational() {
        // 30000/1001 is 29.97. Reading `r_frame_rate` as a plain number yields nothing at all for the most
        // common broadcast rate there is.
        let ntsc = rational(&serde_json::json!("30000/1001")).expect("parse");
        assert!((ntsc - 29.97).abs() < 0.01, "got {ntsc}");
        assert_eq!(rational(&serde_json::json!("10/1")), Some(10.0));
        assert_eq!(rational(&serde_json::json!("0/0")), None);
    }

    #[test]
    fn a_fractional_duration_rounds_rather_than_truncates() {
        let probe = parse(
            r#"{"format":{"duration":"1.500000","format_name":"wav"},
                "streams":[{"codec_type":"audio","codec_name":"pcm_s16le","channels":2,
                            "sample_rate":"48000"}]}"#,
        )
        .expect("parse");
        assert_eq!(probe.duration_ms, Some(1500));
        assert_eq!(probe.sample_rate, Some(48_000));
        assert_eq!(probe.channels, Some(2));
        assert!(probe.is_timed);
    }

    #[test]
    fn a_still_image_is_not_timed_even_though_its_stream_says_video() {
        let probe = parse(
            r#"{"format":{"format_name":"png_pipe"},
                "streams":[{"codec_type":"video","codec_name":"png","width":32,"height":32}]}"#,
        )
        .expect("parse");
        assert!(!probe.is_timed);
        assert_eq!(probe.duration_ms, None);
        assert_eq!((probe.width, probe.height), (Some(32), Some(32)));
    }

    #[test]
    fn a_zero_duration_is_treated_as_no_duration() {
        // Some containers report "0.000000" for a single frame. Zero is not a length, and storing it would
        // make a still look like a video of no duration.
        let probe = parse(
            r#"{"format":{"duration":"0.000000","format_name":"image2"},
                "streams":[{"codec_type":"video","codec_name":"mjpeg","width":8,"height":8}]}"#,
        )
        .expect("parse");
        assert_eq!(probe.duration_ms, None);
        assert!(!probe.is_timed);
    }

    #[test]
    fn output_describing_nothing_usable_is_an_error() {
        let err =
            parse(r#"{"format":{"format_name":"data"},"streams":[]}"#).expect_err("must fail");
        assert!(format!("{err}").contains("neither dimensions"), "got {err}");
    }

    #[test]
    fn the_first_stream_of_each_kind_wins() {
        let probe = parse(
            r#"{"format":{"duration":"5.0"},
                "streams":[{"codec_type":"video","codec_name":"h264","width":100,"height":50},
                           {"codec_type":"video","codec_name":"prores","width":900,"height":900},
                           {"codec_type":"audio","codec_name":"aac","channels":2}]}"#,
        )
        .expect("parse");
        assert_eq!(probe.video_codec.as_deref(), Some("h264"));
        assert_eq!(
            probe.width,
            Some(100),
            "the second video stream must not win"
        );
        assert_eq!(probe.audio_codec.as_deref(), Some("aac"));
    }
}
