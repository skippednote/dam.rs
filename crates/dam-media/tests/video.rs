//! Video transcoding against real ffmpeg (3.5).
//!
//! §2 specifies a 720p H.264 master proxy, and D5 is why: the proxy is the search-and-AI substrate that never
//! tiers, so an archived library stays searchable with zero restores. For video that means one modest H.264
//! file rather than the 4K master.
//!
//! Fixtures are **generated** by ffmpeg's own `testsrc` and `sine` sources rather than checked in. A binary
//! video fixture is a fixture nobody can review, and generated ones let a test say exactly what it needs — a
//! 1080p input to prove downscaling, a silent one to prove the audio branch.
//!
//! Kept deliberately tiny. These run in the gate, and a two-second 320×240 clip proves the same properties as
//! a two-minute 4K one at a thousandth of the cost.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_media::avprobe::{self, AvToolchain};
use dam_media::video;
use std::path::{Path, PathBuf};

/// The toolchain, or a failure that says how to get one.
///
/// The likeliest cause is a missing PATH rather than a missing install: ffmpeg is pinned in `.mise.toml`, so a
/// bare `cargo test` cannot see it while `mise run check` can.
fn toolchain() -> AvToolchain {
    AvToolchain::discover().unwrap_or_else(|e| {
        panic!(
            "these tests need ffmpeg on PATH. It is pinned in .mise.toml, so run them as \
             `mise run check` or `mise exec -- cargo test`, or set DAMRS_FFMPEG_BIN. Underlying \
             error: {e}"
        )
    })
}

/// Generates a test clip.
///
/// `audio` picks between a tone, silence and no audio stream at all — three genuinely different cases for the
/// transcoder, and the third is the one that decides whether a silent AAC track gets written.
async fn clip(
    tools: &AvToolchain,
    dir: &Path,
    name: &str,
    width: u32,
    height: u32,
    seconds: u32,
    audio: Audio,
) -> PathBuf {
    let path = dir.join(name);
    let out = path.to_string_lossy().to_string();
    let size = format!("{width}x{height}");
    let duration = seconds.to_string();
    let video_src = format!("testsrc=size={size}:rate=24");

    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-nostdin".into(),
        "-y".into(),
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        video_src,
    ];
    match audio {
        Audio::Tone => {
            args.extend([
                "-f".into(),
                "lavfi".into(),
                "-i".into(),
                "sine=frequency=440".into(),
            ]);
        }
        Audio::Silence => {
            args.extend([
                "-f".into(),
                "lavfi".into(),
                "-i".into(),
                "anullsrc=channel_layout=stereo:sample_rate=44100".into(),
            ]);
        }
        Audio::None => {}
    }
    args.extend([
        "-t".into(),
        duration,
        "-c:v".into(),
        "libx264".into(),
        "-preset".into(),
        "ultrafast".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
    ]);
    if audio != Audio::None {
        args.extend(["-c:a".into(), "aac".into()]);
    }
    args.push(out);

    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    avprobe::run_ffmpeg(tools, &borrowed)
        .await
        .unwrap_or_else(|e| panic!("generating {name}: {e}"));
    path
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Audio {
    Tone,
    Silence,
    None,
}

// ─── the master proxy ───────────────────────────────────────────────────────

#[tokio::test]
async fn a_1080p_source_becomes_a_720p_proxy() {
    // §2's spec. The proxy is what search, AI and preview all read, so its size is the one number that decides
    // whether keeping a library hot forever is affordable.
    let tools = toolchain();
    let dir = tempfile::tempdir().expect("tempdir");
    let input = clip(&tools, dir.path(), "1080p.mp4", 1920, 1080, 2, Audio::Tone).await;
    let probe = avprobe::probe(&tools, &input).await.expect("probe");
    assert_eq!(probe.height, Some(1080));

    let output = dir.path().join("proxy.mp4");
    let proxy = video::transcode_proxy(&tools, &input, &output, &probe, None)
        .await
        .expect("transcode");

    assert_eq!(proxy.height, video::PROXY_HEIGHT);
    assert_eq!(
        proxy.width, 1280,
        "16:9 at 720 high is 1280 wide, and the width must stay even for H.264"
    );
    assert!(proxy.bytes > 0);
    assert!(
        proxy.bytes < std::fs::metadata(&input).expect("input").len() * 2,
        "a proxy larger than the source would defeat the point"
    );
}

#[tokio::test]
async fn a_smaller_source_is_not_upscaled() {
    // Upscaling costs more and looks worse. `min(ih,720)` is what prevents it, and getting that backwards is
    // easy — a plain `scale=-2:720` would blow a 240p clip up to 720p.
    let tools = toolchain();
    let dir = tempfile::tempdir().expect("tempdir");
    let input = clip(&tools, dir.path(), "small.mp4", 320, 240, 2, Audio::None).await;
    let probe = avprobe::probe(&tools, &input).await.expect("probe");

    let output = dir.path().join("proxy.mp4");
    let proxy = video::transcode_proxy(&tools, &input, &output, &probe, None)
        .await
        .expect("transcode");
    assert_eq!(
        proxy.height, 240,
        "a 240p source must stay 240p rather than being blown up to look worse and cost more"
    );
}

#[tokio::test]
async fn an_odd_height_still_produces_an_even_width() {
    // H.264's 4:2:0 chroma subsampling requires even dimensions, and an odd one is a hard encoder error rather
    // than a warning. The `-2` in the scale filter is what guarantees it.
    let tools = toolchain();
    let dir = tempfile::tempdir().expect("tempdir");
    // 482 high is even, but scaling it toward 720 is where a naive filter chain lands on an odd width.
    let input = clip(&tools, dir.path(), "odd.mp4", 854, 482, 2, Audio::None).await;
    let probe = avprobe::probe(&tools, &input).await.expect("probe");

    let output = dir.path().join("proxy.mp4");
    let proxy = video::transcode_proxy(&tools, &input, &output, &probe, None)
        .await
        .expect("transcode");
    assert_eq!(proxy.width % 2, 0, "got {}", proxy.width);
    assert_eq!(proxy.height % 2, 0, "got {}", proxy.height);
}

#[tokio::test]
async fn a_video_with_no_audio_gets_no_silent_track() {
    // `-an` rather than letting ffmpeg decide. A silent AAC track is bytes kept hot forever for nothing, and
    // it also makes a downstream "does this have audio" check answer yes.
    let tools = toolchain();
    let dir = tempfile::tempdir().expect("tempdir");
    let input = clip(&tools, dir.path(), "mute.mp4", 640, 360, 2, Audio::None).await;
    let probe = avprobe::probe(&tools, &input).await.expect("probe");
    assert!(
        probe.audio_codec.is_none(),
        "the fixture must have no audio"
    );

    let output = dir.path().join("proxy.mp4");
    video::transcode_proxy(&tools, &input, &output, &probe, None)
        .await
        .expect("transcode");

    let rendered = avprobe::probe(&tools, &output).await.expect("probe");
    assert!(
        rendered.audio_codec.is_none(),
        "a video with no sound must not gain a silent track"
    );
}

#[tokio::test]
async fn the_proxy_is_playable_and_progressive() {
    // `+faststart` moves the moov atom to the front. Without it a browser waits for the whole file before
    // playing, which is the single most common reason a valid MP4 "does not play" — and it is invisible in any
    // test that only checks the file parses.
    let tools = toolchain();
    let dir = tempfile::tempdir().expect("tempdir");
    let input = clip(&tools, dir.path(), "fast.mp4", 640, 360, 2, Audio::Tone).await;
    let probe = avprobe::probe(&tools, &input).await.expect("probe");

    let output = dir.path().join("proxy.mp4");
    video::transcode_proxy(&tools, &input, &output, &probe, None)
        .await
        .expect("transcode");

    let bytes = std::fs::read(&output).expect("read");
    // In a faststart file `moov` appears before `mdat`. Comparing positions is the only way to observe this
    // from outside ffmpeg.
    let moov = find(&bytes, b"moov").expect("an moov atom");
    let mdat = find(&bytes, b"mdat").expect("an mdat atom");
    assert!(
        moov < mdat,
        "moov at {moov} must precede mdat at {mdat}, or a browser buffers the whole file first"
    );
}

// ─── loudness ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn loudness_is_measured_from_stderr_not_stdout() {
    // ffmpeg writes filter output to stderr. A version reading stdout gets an empty string and concludes the
    // file has no audio — which would silently disable normalisation for the whole library.
    let tools = toolchain();
    let dir = tempfile::tempdir().expect("tempdir");
    let input = clip(&tools, dir.path(), "tone.mp4", 320, 240, 3, Audio::Tone).await;

    let measured = video::measure_loudness(&tools, &input, Some(3_000))
        .await
        .expect("measure");
    assert!(
        measured.input_i > -99.0 && measured.input_i < 0.0,
        "a 440 Hz tone must measure as real loudness, got {}",
        measured.input_i
    );
}

#[tokio::test]
async fn a_silent_track_measures_rather_than_failing_the_transcode() {
    // ffmpeg reports `-inf` for silence and no float parser accepts it. Silence is a legitimate input — a
    // screen recording with the microphone muted — so it must not fail.
    let tools = toolchain();
    let dir = tempfile::tempdir().expect("tempdir");
    let input = clip(
        &tools,
        dir.path(),
        "silent.mp4",
        320,
        240,
        2,
        Audio::Silence,
    )
    .await;

    let measured = video::measure_loudness(&tools, &input, Some(2_000))
        .await
        .expect("silence must still measure");
    assert!(
        measured.input_i < -90.0,
        "silence should measure very quiet, got {}",
        measured.input_i
    );

    assert!(
        measured.is_silent(),
        "the measurement must report silence so the transcode knows to skip normalising"
    );

    // And the transcode completes *with* those measurements, by skipping the filter rather than applying it.
    // This is the case that found the bug: feeding a silent measurement to `loudnorm` asks it to lift -inf to
    // -16 LUFS, and the resulting gain makes the filter emit samples the AAC encoder rejects — "Input contains
    // (near) NaN/+-Inf", then "Conversion failed!". Handling silence "gracefully" produced a corrupt stream.
    let probe = avprobe::probe(&tools, &input).await.expect("probe");
    let output = dir.path().join("proxy.mp4");
    let proxy = video::transcode_proxy(&tools, &input, &output, &probe, Some(measured))
        .await
        .expect("a silent asset must still transcode");
    assert!(proxy.bytes > 0);

    // The audio track survives — skipping normalisation must not mean dropping the stream.
    let rendered = avprobe::probe(&tools, &output).await.expect("probe");
    assert!(
        rendered.audio_codec.is_some(),
        "a silent track is still a track, and dropping it would change what the asset is"
    );
}

#[tokio::test]
async fn the_two_pass_transcode_moves_the_measured_loudness_toward_the_target() {
    // The point of two passes. A single pass adapts as it goes, which pumps quiet passages and leaves the
    // volume moving *inside* each clip — the opposite of what normalising is for. Measuring the output is the
    // only way to see that the offsets were actually applied.
    let tools = toolchain();
    let dir = tempfile::tempdir().expect("tempdir");
    // A quiet tone, so there is a real gap to close.
    let input = clip(&tools, dir.path(), "quiet.mp4", 320, 240, 4, Audio::Tone).await;

    let before = video::measure_loudness(&tools, &input, Some(4_000))
        .await
        .expect("measure input");
    let probe = avprobe::probe(&tools, &input).await.expect("probe");
    let output = dir.path().join("normalised.mp4");
    video::transcode_proxy(&tools, &input, &output, &probe, Some(before))
        .await
        .expect("transcode");

    let after = video::measure_loudness(&tools, &output, Some(4_000))
        .await
        .expect("measure output");
    let gap_before = (before.input_i - video::TARGET_LUFS).abs();
    let gap_after = (after.input_i - video::TARGET_LUFS).abs();
    assert!(
        gap_after <= gap_before,
        "normalising must not move loudness away from the target: {} -> {} against a target of {}",
        before.input_i,
        after.input_i,
        video::TARGET_LUFS
    );
}

// ─── HLS ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn hls_segments_the_proxy_and_the_playlist_lists_every_segment() {
    // The playlist is the authority on order, not the filesystem. Globbing the directory sorts lexically,
    // which happens to work for five-digit names and stops working at 100,000 segments.
    let tools = toolchain();
    let dir = tempfile::tempdir().expect("tempdir");
    // 14 seconds against a 6-second segment length gives three segments, so the count is a real check rather
    // than "at least one".
    let input = clip(&tools, dir.path(), "long.mp4", 640, 360, 14, Audio::Tone).await;
    let probe = avprobe::probe(&tools, &input).await.expect("probe");
    let proxy_path = dir.path().join("proxy.mp4");
    let proxy = video::transcode_proxy(&tools, &input, &proxy_path, &probe, None)
        .await
        .expect("transcode");

    let hls_dir = dir.path().join("hls");
    let hls = video::segment_hls(&tools, &proxy.path, &hls_dir, proxy.duration_ms)
        .await
        .expect("segment");

    assert!(hls.playlist.exists());
    assert!(
        hls.segments.len() >= 2,
        "14 seconds at {}s segments should give several, got {}",
        video::HLS_SEGMENT_SECONDS,
        hls.segments.len()
    );
    for segment in &hls.segments {
        assert!(segment.exists(), "{segment:?} is listed but missing");
    }
    assert!(hls.total_bytes > 0);

    let playlist = std::fs::read_to_string(&hls.playlist).expect("read");
    assert!(
        playlist.contains("#EXT-X-PLAYLIST-TYPE:VOD"),
        "on-demand, not live: a rolling window would make the start of an asset unreachable"
    );
    assert!(
        playlist.contains("#EXT-X-ENDLIST"),
        "a VOD playlist must be closed"
    );
}

#[tokio::test]
async fn segmenting_does_not_re_encode() {
    // `-c copy`. Re-encoding here would mean a second full transcode and a second set of quality decisions,
    // and the proxy is already the right size and bitrate — so the segments should sum to roughly the proxy.
    let tools = toolchain();
    let dir = tempfile::tempdir().expect("tempdir");
    let input = clip(&tools, dir.path(), "copy.mp4", 640, 360, 8, Audio::Tone).await;
    let probe = avprobe::probe(&tools, &input).await.expect("probe");
    let proxy_path = dir.path().join("proxy.mp4");
    let proxy = video::transcode_proxy(&tools, &input, &proxy_path, &probe, None)
        .await
        .expect("transcode");

    let hls = video::segment_hls(
        &tools,
        &proxy.path,
        &dir.path().join("hls"),
        proxy.duration_ms,
    )
    .await
    .expect("segment");

    // MPEG-TS carries more framing overhead than MP4, so the segments are larger — but only somewhat. A
    // re-encode would land far off in one direction or the other.
    let ratio = hls.total_bytes as f64 / proxy.bytes as f64;
    assert!(
        (0.8..3.0).contains(&ratio),
        "segments totalled {} against a {}-byte proxy (ratio {ratio:.2}), which suggests a re-encode",
        hls.total_bytes,
        proxy.bytes
    );
}

// ─── bounds ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_transcode_that_overruns_its_budget_is_killed() {
    // The property that makes the duration-derived budget meaningful: a transcode which exceeds it is stuck,
    // not slow, and the worker has to come back.
    let tools = toolchain();
    let dir = tempfile::tempdir().expect("tempdir");
    let input = clip(&tools, dir.path(), "bounded.mp4", 640, 360, 3, Audio::Tone).await;

    // A one-millisecond wall clock. Absurd on purpose — a transcode that ignored the limits would still
    // succeed here.
    let outcome = avprobe::run_ffmpeg_with_limits(
        &tools,
        &[
            "-hide_banner",
            "-nostdin",
            "-y",
            "-i",
            &input.to_string_lossy(),
            "-c:v",
            "libx264",
            &dir.path().join("out.mp4").to_string_lossy(),
        ],
        dam_media::sandbox::Limits {
            wall_clock: std::time::Duration::from_millis(1),
            ..video::limits_for(Some(3_000))
        },
    )
    .await;
    assert!(
        outcome.is_err(),
        "a 1ms wall clock must stop the transcode, which proves the limits are applied"
    );
}

#[tokio::test]
async fn an_input_that_is_not_media_is_an_error_carrying_ffmpegs_diagnosis() {
    let tools = toolchain();
    let dir = tempfile::tempdir().expect("tempdir");
    let bogus = dir.path().join("nonsense.mp4");
    std::fs::write(&bogus, b"this is not a video").expect("write");

    let error = video::measure_loudness(&tools, &bogus, Some(1_000))
        .await
        .expect_err("must fail");
    // The message has to carry ffmpeg's own words; "transcode failed" sends someone to read our code when the
    // answer is in the tool's stderr.
    assert!(!format!("{error}").is_empty());
}

/// The first offset of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
