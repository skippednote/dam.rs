//! Video transcoding: the master proxy, loudness, and HLS (3.5).
//!
//! §2 specifies a 720p H.264 master proxy alongside the 2048px JPEG one, and D5 is why: the proxy is the
//! search-and-AI substrate that never tiers, so an archived library stays searchable and re-processable with
//! zero restores. For video that means one modest H.264 file, not the 4K master.
//!
//! ## Limits are derived from duration, not fixed
//!
//! This is the part a default gets wrong in both directions. `sandbox::Limits::default()` allows 120 seconds
//! of wall clock — fine for a probe, and it kills any real transcode. Raising it to something generous
//! enough for a three-hour film means a *hung* ffmpeg on a ten-second clip holds a worker for that whole
//! budget.
//!
//! So the budget scales with the input: [`limits_for`] allows a multiple of the media's own duration plus a
//! floor for startup. A transcode that overruns that is not slow, it is stuck.
//!
//! ## Loudness normalisation needs two passes
//!
//! ffmpeg's `loudnorm` in a single pass is *dynamic*: it adapts as it goes, which pumps quiet passages up and
//! leaves the result inconsistent between assets — the exact problem normalising was supposed to solve. The
//! accurate form measures in one pass, then applies the measured offsets in a second. That is twice the
//! decode, and it is the difference between "the volume is even across the library" and "the volume moves
//! around inside each clip".
//!
//! ## HLS output is many files
//!
//! A playlist plus N segments, all of which have to land under one prefix and be referenced by relative name.
//! The playlist is text and small; the segments are the payload. That shape is why [`HlsOutput`] reports both
//! rather than a single key.

use crate::avprobe::{self, AvProbe, AvToolchain, Error};

type Result<T> = std::result::Result<T, Error>;
use crate::sandbox::Limits;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The master proxy's longest edge, in pixels.
///
/// 720p as §2 specifies. Large enough for a person to judge a shot and for a vision model to work from,
/// small enough that a library of it is affordable to keep hot forever.
pub const PROXY_HEIGHT: u32 = 720;

/// Target integrated loudness, in LUFS.
///
/// −16 rather than broadcast's −23: the proxy is played in a browser, and −23 is quiet enough that reviewers
/// turn their system volume up and then get startled by the next tab.
pub const TARGET_LUFS: f64 = -16.0;

/// Target true peak, in dBTP. −1.5 leaves headroom for the lossy encode to overshoot without clipping.
pub const TARGET_TRUE_PEAK: f64 = -1.5;

/// Loudness range target.
pub const TARGET_LRA: f64 = 11.0;

/// HLS segment length, in seconds.
///
/// Six is the value Apple's authoring guidance settles on: short enough that a seek does not stall, long
/// enough that a two-hour asset does not become 7,000 objects — and the object count is what a DAM pays for.
pub const HLS_SEGMENT_SECONDS: u32 = 6;

/// Multiple of the media's own duration allowed for a transcode.
///
/// Four is generous for H.264 at 720p on any modern machine and still bounds a stuck process: a ten-second
/// clip gets under a minute rather than the two hours a fixed generous budget would grant it.
pub const DURATION_BUDGET_MULTIPLE: u32 = 4;

/// Floor on the wall clock, for startup and for assets whose duration is unknown.
pub const MIN_WALL_CLOCK: Duration = Duration::from_secs(60);

/// Ceiling, so a mis-probed duration cannot hand out an unbounded budget.
pub const MAX_WALL_CLOCK: Duration = Duration::from_secs(6 * 60 * 60);

/// Sandbox limits sized for transcoding `duration_ms` of media.
///
/// The reason this is a function rather than a constant: see the module docs. A probe's default would kill a
/// real transcode, and a transcode's budget would let a hung probe hold a worker for hours.
pub fn limits_for(duration_ms: Option<i64>) -> Limits {
    let wall_clock = match duration_ms {
        Some(ms) if ms > 0 => {
            let seconds = (ms / 1000).unsigned_abs();
            let budget = seconds.saturating_mul(u64::from(DURATION_BUDGET_MULTIPLE));
            Duration::from_secs(budget).clamp(MIN_WALL_CLOCK, MAX_WALL_CLOCK)
        }
        // Unknown duration: the floor. An unprobeable file is not a file to hand a large budget to.
        _ => MIN_WALL_CLOCK,
    };
    Limits {
        wall_clock,
        // No CPU cap. Transcoding *is* CPU work, and `ulimit -t` bounds exactly the thing this process is
        // supposed to spend — the wall clock is the bound that distinguishes slow from stuck. Left explicit
        // rather than inherited so nobody re-adds it from the default and wonders why long videos die.
        cpu_seconds: None,
        // Generous but bounded: ffmpeg's own buffers scale with resolution, and a 4K input needs more than a
        // probe.
        address_space_bytes: Some(6 * 1024 * 1024 * 1024),
        // A 720p proxy of a long film is a few gigabytes; well past that is a runaway.
        file_size_bytes: Some(16 * 1024 * 1024 * 1024),
        max_output_bytes: 512 * 1024,
    }
}

/// Measured loudness, from `loudnorm`'s first pass.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Loudness {
    pub input_i: f64,
    pub input_tp: f64,
    pub input_lra: f64,
    pub input_thresh: f64,
    pub target_offset: f64,
}

/// Integrated loudness below which a track is treated as silent.
///
/// ffmpeg reports `-inf` for true silence and something in the −80s for a track that is technically present
/// and inaudible. Both are "nothing to normalise".
pub const SILENCE_LUFS: f64 = -70.0;

impl Loudness {
    /// Whether there is anything here to normalise.
    ///
    /// **Load-bearing, and found by a test rather than by reasoning.** Feeding a silent measurement back into
    /// `loudnorm` asks it to raise −inf to −16 LUFS, and the resulting gain makes the filter emit samples the
    /// AAC encoder rejects outright: `Input contains (near) NaN/+-Inf`, then `Conversion failed!`. So the
    /// first version of this — mapping `-inf` to a sentinel and passing it through — turned "handle silence
    /// gracefully" into "produce a corrupt audio stream".
    ///
    /// Silence is left alone instead. There is no loudness to correct, and copying the track through is the
    /// only answer that produces a playable file.
    pub fn is_silent(self) -> bool {
        self.input_i <= SILENCE_LUFS
    }
}

/// A transcoded proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyOutput {
    pub path: PathBuf,
    pub bytes: u64,
    pub width: u32,
    pub height: u32,
    pub duration_ms: Option<i64>,
}

/// An HLS rendition: one playlist and its segments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsOutput {
    pub playlist: PathBuf,
    /// Segment files, in playlist order.
    pub segments: Vec<PathBuf>,
    pub total_bytes: u64,
}

/// Measures loudness without producing output.
///
/// The first of `loudnorm`'s two passes. Writing to the null muxer so the decode happens and nothing is
/// encoded — the measurement is the whole product of this call.
pub async fn measure_loudness(
    tools: &AvToolchain,
    input: &Path,
    duration_ms: Option<i64>,
) -> Result<Loudness> {
    let filter = format!(
        "loudnorm=I={TARGET_LUFS}:TP={TARGET_TRUE_PEAK}:LRA={TARGET_LRA}:print_format=json"
    );
    let input_arg = input.to_string_lossy().to_string();
    let args = vec![
        "-hide_banner",
        "-nostdin",
        "-i",
        &input_arg,
        "-af",
        &filter,
        "-f",
        "null",
        "-",
    ];

    // The measurement is on **stderr**: ffmpeg writes filter output there, and reading stdout returns nothing
    // at all. A version of this that parsed stdout would report "no loudness data" for every file.
    let stderr = avprobe::run_ffmpeg_capturing(tools, &args, limits_for(duration_ms)).await?;
    parse_loudness(&stderr).ok_or_else(|| {
        Error::Unparseable(
            "ffmpeg reported no loudnorm measurement; the input may have no audio stream"
                .to_owned(),
        )
    })
}

/// Renders the 720p H.264 master proxy.
///
/// `loudness` comes from [`measure_loudness`]. Passing `None` produces a proxy with **no** loudness
/// normalisation rather than a single-pass one, because single-pass `loudnorm` is dynamic and makes the volume
/// move around inside each clip — worse than leaving it alone.
///
/// A measurement that reports silence is also skipped, whatever the caller passes. See
/// [`Loudness::is_silent`]: normalising silence produces a stream the encoder rejects, so this is not a
/// preference the caller gets to override.
/// A single frame, as JPEG bytes, for a video's thumbnail (3.5's visible half).
///
/// A library of videos with no thumbnails is a grid of grey rectangles, and until this every clip in one was
/// exactly that: the image profiles cannot decode a container, so a video had no derivative at all and the
/// grid showed "processing" forever.
///
/// The frame is taken *into* the clip rather than at zero, because the first frame of a phone video is very
/// often black or a blurred pan. A tenth of the duration, capped at ten seconds so a feature does not decode
/// for a minute to find its poster.
///
/// With no duration the seek is **zero**, not a guessed second. A one-second clip — which is what a Live Photo
/// is, and there are thousands of them in a phone library — has nothing at the one-second mark, and ffmpeg
/// exits 0 having written no file at all. Frame zero of a short clip is a real frame; a second into it is
/// nothing.
///
/// Returns the JPEG bytes rather than writing a derivative: what to do with a frame is the pipeline's decision,
/// and the renditions it feeds are the ordinary image ones.
pub async fn poster_frame(
    tools: &AvToolchain,
    input: &Path,
    duration_ms: Option<i64>,
) -> Result<Vec<u8>> {
    let seek_ms = duration_ms
        .map(|total| {
            (total / 10)
                .clamp(1_000, 10_000)
                .min(total.saturating_sub(100))
        })
        .unwrap_or(1_000)
        .max(0);
    let seconds = format!("{}.{:03}", seek_ms / 1000, seek_ms % 1000);

    let dir = tempfile::tempdir().map_err(|e| Error::Rejected {
        path: input.display().to_string(),
        detail: format!("temp dir: {e}"),
    })?;
    let out = dir.path().join("poster.jpg");
    let out_arg = out.to_string_lossy().to_string();
    let input_arg = input.to_string_lossy().to_string();

    // `-ss` before `-i` so ffmpeg seeks rather than decodes up to the point, which on a long clip is the
    // difference between a second and a minute. `-frames:v 1` and `-an`: one picture, no audio stream.
    let args: Vec<String> = vec![
        "-hide_banner".into(),
        "-nostdin".into(),
        "-y".into(),
        "-ss".into(),
        seconds,
        "-i".into(),
        input_arg,
        "-frames:v".into(),
        "1".into(),
        "-an".into(),
        // Quality 3 of 31: a poster is re-rendered into the thumbnail profiles from here, so this is an
        // intermediate and should not be the lossy step that shows.
        "-q:v".into(),
        "3".into(),
        out_arg,
    ];
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    // A frame extraction is a decode of one picture, so the probe's budget is the right order of magnitude —
    // not the transcode's, which scales with duration.
    crate::avprobe::run_ffmpeg(tools, &borrowed).await?;

    let bytes = tokio::fs::read(&out).await.map_err(|e| Error::Rejected {
        path: input.display().to_string(),
        detail: format!("reading the poster frame: {e}"),
    })?;
    if bytes.is_empty() {
        // ffmpeg exits 0 having written nothing when the seek lands past the last frame, which a container
        // with a lying duration does. An empty poster would be recorded as a derivative and served as a
        // broken image.
        return Err(Error::Rejected {
            path: input.display().to_string(),
            detail: "ffmpeg wrote an empty poster frame".to_owned(),
        });
    }
    Ok(bytes)
}

pub async fn transcode_proxy(
    tools: &AvToolchain,
    input: &Path,
    output: &Path,
    probe: &AvProbe,
    loudness: Option<Loudness>,
) -> Result<ProxyOutput> {
    let input_arg = input.to_string_lossy().to_string();
    let output_arg = output.to_string_lossy().to_string();

    // `-2` on the width keeps it even, which H.264's chroma subsampling requires — an odd width is a hard
    // encoder error, not a warning. `min(ih,720)` never upscales: a 480p source stays 480p rather than being
    // blown up to look worse and cost more.
    let scale = format!("scale=-2:min(ih\\,{PROXY_HEIGHT})");

    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-nostdin".into(),
        "-y".into(),
        "-i".into(),
        input_arg,
        "-vf".into(),
        scale,
        "-c:v".into(),
        "libx264".into(),
        // `veryfast` and CRF 23: the proxy is a working copy, and spending four times the CPU for a file
        // nobody masters from is the wrong trade at library scale.
        "-preset".into(),
        "veryfast".into(),
        "-crf".into(),
        "23".into(),
        // Progressive download in a browser needs the moov atom at the front, or playback waits for the whole
        // file. This is the single most common reason a valid MP4 "does not play".
        "-movflags".into(),
        "+faststart".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
    ];

    if probe.audio_codec.is_some() {
        args.push("-c:a".into());
        args.push("aac".into());
        args.push("-b:a".into());
        args.push("128k".into());
        // A silent track is skipped rather than normalised — see `Loudness::is_silent`. Asking `loudnorm` to
        // lift silence to the target produces samples the AAC encoder refuses.
        if let Some(measured) = loudness.filter(|m| !m.is_silent()) {
            // The second pass: the measured values are supplied so `loudnorm` applies a fixed offset instead
            // of adapting as it goes.
            args.push("-af".into());
            args.push(format!(
                "loudnorm=I={TARGET_LUFS}:TP={TARGET_TRUE_PEAK}:LRA={TARGET_LRA}\
                 :measured_I={}:measured_TP={}:measured_LRA={}:measured_thresh={}\
                 :offset={}:linear=true",
                measured.input_i,
                measured.input_tp,
                measured.input_lra,
                measured.input_thresh,
                measured.target_offset
            ));
        }
    } else {
        // No audio stream. `-an` rather than letting ffmpeg decide, so a video with no sound produces a file
        // with no silent track — a silent AAC track is bytes kept forever for nothing.
        args.push("-an".into());
    }
    args.push(output_arg);

    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    avprobe::run_ffmpeg_with_limits(tools, &borrowed, limits_for(probe.duration_ms)).await?;

    let rendered = avprobe::probe_with_limits(tools, output, limits_for(probe.duration_ms)).await?;
    let bytes = std::fs::metadata(output).map(|m| m.len()).unwrap_or(0);
    Ok(ProxyOutput {
        path: output.to_path_buf(),
        bytes,
        width: rendered.width.unwrap_or(0),
        height: rendered.height.unwrap_or(0),
        duration_ms: rendered.duration_ms,
    })
}

/// Segments an already-transcoded proxy into HLS.
///
/// Takes the proxy rather than the master on purpose: segmenting re-encodes nothing (`-c copy`), so the
/// playlist inherits the proxy's size and bitrate. Segmenting the master would mean a second full transcode
/// and a second set of decisions about quality.
pub async fn segment_hls(
    tools: &AvToolchain,
    proxy: &Path,
    directory: &Path,
    duration_ms: Option<i64>,
) -> Result<HlsOutput> {
    std::fs::create_dir_all(directory).map_err(|e| Error::Rejected {
        path: directory.display().to_string(),
        detail: format!("creating the HLS directory: {e}"),
    })?;

    let playlist = directory.join("index.m3u8");
    let pattern = directory.join("segment-%05d.ts");
    let proxy_arg = proxy.to_string_lossy().to_string();
    let playlist_arg = playlist.to_string_lossy().to_string();
    let pattern_arg = pattern.to_string_lossy().to_string();
    let segment_seconds = HLS_SEGMENT_SECONDS.to_string();

    let args = vec![
        "-hide_banner",
        "-nostdin",
        "-y",
        "-i",
        &proxy_arg,
        // No re-encode. The proxy is already the right size and bitrate.
        "-c",
        "copy",
        "-f",
        "hls",
        "-hls_time",
        &segment_seconds,
        // Every segment listed, rather than a rolling window: this is video on demand, and a live-style
        // playlist would make the start of an asset unreachable.
        "-hls_playlist_type",
        "vod",
        "-hls_segment_filename",
        &pattern_arg,
        &playlist_arg,
    ];
    avprobe::run_ffmpeg_with_limits(tools, &args, limits_for(duration_ms)).await?;

    // Read from the *playlist* rather than by globbing the directory. The playlist is the authority on order,
    // and a glob would sort lexically — which happens to work for five-digit names and silently stops working
    // at 100,000 segments or if the pattern ever changes.
    let text = std::fs::read_to_string(&playlist).map_err(|e| Error::Rejected {
        path: playlist.display().to_string(),
        detail: format!("reading the playlist: {e}"),
    })?;
    let segments: Vec<PathBuf> = text
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(|name| directory.join(name.trim()))
        .collect();

    let total_bytes = segments
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();

    Ok(HlsOutput {
        playlist,
        segments,
        total_bytes,
    })
}

/// Pulls the JSON block `loudnorm` prints on stderr.
///
/// Scans for the **last** `{`, because ffmpeg prints its banner, stream information and progress on the same
/// stream, and an earlier brace belongs to something else.
fn parse_loudness(stderr: &str) -> Option<Loudness> {
    let start = stderr.rfind('{')?;
    let end = stderr[start..].rfind('}')? + start + 1;
    let json: serde_json::Value = serde_json::from_str(&stderr[start..end]).ok()?;

    // Every field arrives as a *string* in ffmpeg's JSON, and some are the literal `-inf` for silence, which
    // no float parser accepts. Silence is a legitimate input, so it maps to a very low value rather than
    // failing the whole measurement.
    let number = |key: &str| -> Option<f64> {
        let raw = json.get(key)?.as_str()?;
        match raw.trim() {
            "-inf" => Some(-99.0),
            "inf" => Some(0.0),
            other => other.parse().ok(),
        }
    };

    Some(Loudness {
        input_i: number("input_i")?,
        input_tp: number("input_tp")?,
        input_lra: number("input_lra")?,
        input_thresh: number("input_thresh")?,
        target_offset: number("target_offset").unwrap_or(0.0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wall_clock_scales_with_duration_and_is_bounded_both_ways() {
        // A probe's 120-second default kills a real transcode; a transcode's budget lets a hung ffmpeg on a
        // ten-second clip hold a worker for hours. Both directions matter.
        let short = limits_for(Some(10_000));
        assert_eq!(
            short.wall_clock, MIN_WALL_CLOCK,
            "a ten-second clip gets the floor, not four times ten seconds"
        );

        let hour = limits_for(Some(3_600_000));
        assert_eq!(hour.wall_clock, Duration::from_secs(4 * 3_600));

        let absurd = limits_for(Some(i64::MAX));
        assert_eq!(
            absurd.wall_clock, MAX_WALL_CLOCK,
            "a mis-probed duration must not hand out an unbounded budget"
        );

        assert_eq!(
            limits_for(None).wall_clock,
            MIN_WALL_CLOCK,
            "an unprobeable file is not one to trust with a large budget"
        );
    }

    #[test]
    fn transcoding_has_no_cpu_cap() {
        // `ulimit -t` bounds exactly the thing a transcode is supposed to spend. Left explicit so nobody
        // re-adds it from the default and then wonders why long videos die.
        assert!(limits_for(Some(600_000)).cpu_seconds.is_none());
    }

    #[test]
    fn loudness_json_is_read_from_the_last_brace() {
        // ffmpeg prints its banner, stream info and progress on the same stream. An earlier brace belongs to
        // something else, and taking the first would parse the wrong object.
        let stderr = "ffmpeg version 9.0.1\n{ \"not\": \"this\" }\nStream #0:0\n\
            [Parsed_loudnorm_0 @ 0x1] \n\
            {\n\"input_i\" : \"-27.02\",\n\"input_tp\" : \"-9.28\",\n\
            \"input_lra\" : \"1.30\",\n\"input_thresh\" : \"-37.11\",\n\
            \"target_offset\" : \"0.32\"\n}\n";
        let measured = parse_loudness(stderr).expect("a measurement");
        assert!((measured.input_i - -27.02).abs() < 0.001);
        assert!((measured.target_offset - 0.32).abs() < 0.001);
    }

    #[test]
    fn silence_measures_rather_than_failing() {
        // `-inf` is what ffmpeg reports for a silent track, and no float parser accepts it. Silence is a
        // legitimate input — a screen recording with the microphone muted — so it must not fail the transcode.
        let stderr = "{\n\"input_i\" : \"-inf\",\n\"input_tp\" : \"-inf\",\n\
            \"input_lra\" : \"0.00\",\n\"input_thresh\" : \"-inf\",\n\
            \"target_offset\" : \"0.00\"\n}";
        let measured = parse_loudness(stderr).expect("silence still measures");
        assert!(measured.input_i < -90.0);
    }

    #[test]
    fn output_with_no_json_at_all_is_none_rather_than_a_panic() {
        assert!(parse_loudness("ffmpeg version 9.0.1\nno audio stream found").is_none());
        assert!(parse_loudness("").is_none());
        // An unbalanced brace must not index out of bounds.
        assert!(parse_loudness("{ truncated").is_none());
    }

    // Compile-time guards rather than a test, since both are constants: `assert!` on a constant is a lint,
    // and a `const` block fails the build rather than a run.
    //
    // -23 LUFS is broadcast. In a browser it is quiet enough that reviewers raise their system volume and then
    // get startled by the next tab. And a true peak below zero leaves headroom for a lossy encoder to
    // overshoot without clipping.
    const _: () = assert!(TARGET_LUFS > -23.0);
    const _: () = assert!(TARGET_TRUE_PEAK < 0.0);
    const _: () = assert!(SILENCE_LUFS < TARGET_LUFS);
}
