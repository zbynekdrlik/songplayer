//! Unit tests for the pure preview-encoder helpers (#178) — ladder selection,
//! `-encoders` parse, and the exact ffmpeg argument vector. Linux-runnable.

use super::*;

#[test]
fn select_encoder_prefers_hardware_in_ladder_order() {
    // nvenc wins over everything else present.
    assert_eq!(
        select_encoder(&["libx264".into(), "h264_qsv".into(), "h264_nvenc".into(),]),
        "h264_nvenc"
    );
    // qsv wins when nvenc is absent.
    assert_eq!(
        select_encoder(&["libx264".into(), "h264_amf".into(), "h264_qsv".into()]),
        "h264_qsv"
    );
    // amf wins over software only.
    assert_eq!(
        select_encoder(&["libx264".into(), "h264_amf".into()]),
        "h264_amf"
    );
    // Software fallback when only libx264 is present.
    assert_eq!(select_encoder(&["libx264".into()]), "libx264");
    // Empty availability still yields libx264 (never panics / never empty).
    assert_eq!(select_encoder(&[]), "libx264");
}

#[test]
fn parse_available_encoders_keeps_only_ladder_names() {
    let sample = "\
Encoders:
 V..... = Video
 ------
 V....D h264_nvenc           NVIDIA NVENC H.264 encoder
 V....D h264_qsv             H.264 (Intel Quick Sync Video)
 V....D libx264              libx264 H.264 / AVC
 A....D aac                  AAC (Advanced Audio Coding)
 V....D hevc_nvenc           NVIDIA NVENC hevc encoder
";
    let found = parse_available_encoders(sample);
    // aac + hevc_nvenc are not in the ladder; order follows first appearance.
    assert_eq!(found, vec!["h264_nvenc", "h264_qsv", "libx264"]);
}

#[test]
fn parse_available_encoders_dedups_and_ignores_blank_lines() {
    let sample = " V....D libx264 x\n\n V....D libx264 y\n";
    assert_eq!(parse_available_encoders(sample), vec!["libx264"]);
    assert!(parse_available_encoders("").is_empty());
}

#[test]
fn ffmpeg_args_are_exact_for_libx264_with_low_latency_tuning() {
    // #221: no input is wall-clock stamped. The rawvideo input counts frames
    // at 25 fps (`-framerate 25`), the f32le PCM input counts samples, and both
    // are written on SongPlayer's monotonic clock. There is no -itsoffset
    // (#178 round 3).
    let args = build_ffmpeg_args(5001, 5002, "libx264");
    let expected: Vec<String> = [
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "rawvideo",
        "-pix_fmt",
        "nv12",
        "-s",
        "640x360",
        "-framerate",
        "25",
        "-i",
        "tcp://127.0.0.1:5001",
        "-f",
        "f32le",
        "-ar",
        "48000",
        "-ac",
        "2",
        "-i",
        "tcp://127.0.0.1:5002",
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-tune",
        "zerolatency",
        "-b:v",
        "500k",
        "-maxrate",
        "500k",
        "-bufsize",
        "500k",
        "-g",
        "25",
        "-fps_mode",
        "cfr",
        "-r",
        "25",
        "-c:a",
        "aac",
        "-b:a",
        "64k",
        "-movflags",
        "+frag_keyframe+empty_moov+default_base_moof",
        "-frag_duration",
        "500000",
        "-flush_packets",
        "1",
        "-f",
        "mp4",
        "pipe:1",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(args, expected);
}

#[test]
fn ffmpeg_args_fit_a_1mbps_remote_uplink() {
    // #184 round F: the ~1.35 Mb/s stream (-b:v 1200k + -b:a 128k) did not fit a
    // ~1 Mb/s internet uplink, so the remote preview back-pressured and fell
    // permanently behind. Video is capped at 500k with a bounded overshoot
    // (-maxrate 500k -bufsize 500k) and audio at 64k -> ~0.6 Mb/s, and the old
    // 1200k / 128k values must be gone entirely (hardware AND software paths).
    for encoder in ["libx264", "h264_nvenc"] {
        let args = build_ffmpeg_args(6001, 6002, encoder);
        let has = |flag: &str, val: &str| args.windows(2).any(|w| w[0] == flag && w[1] == val);
        assert!(has("-b:v", "500k"), "video bitrate 500k for {encoder}");
        assert!(has("-maxrate", "500k"), "-maxrate 500k for {encoder}");
        assert!(has("-bufsize", "500k"), "-bufsize 500k for {encoder}");
        assert!(has("-b:a", "64k"), "audio bitrate 64k for {encoder}");
        assert!(
            !args.iter().any(|a| a == "1200k"),
            "the old 1200k video bitrate is gone for {encoder}"
        );
        assert!(
            !args.iter().any(|a| a == "128k"),
            "the old 128k audio bitrate is gone for {encoder}"
        );
    }
}

#[test]
fn ffmpeg_args_omit_libx264_tuning_for_hardware_encoders() {
    let args = build_ffmpeg_args(1, 2, "h264_nvenc");
    // The hardware path uses the codec directly with no -preset/-tune.
    assert!(args.contains(&"h264_nvenc".to_string()));
    assert!(!args.contains(&"-preset".to_string()));
    assert!(!args.contains(&"zerolatency".to_string()));
    // The two loopback URLs carry the given ports and canvas size is fixed.
    assert!(args.contains(&"tcp://127.0.0.1:1".to_string()));
    assert!(args.contains(&"tcp://127.0.0.1:2".to_string()));
    assert!(args.contains(&"640x360".to_string()));
    // Still muxes fragmented MP4 to stdout.
    assert_eq!(args.last().unwrap(), "pipe:1");
}

#[test]
fn ffmpeg_args_never_contain_itsoffset() {
    // #178 round 3: `-itsoffset` is REMOVED (the box ffmpeg kept the audio
    // start_time at 0.000 regardless of it, so it was not a dependable A/V lever
    // — alignment is done with a silence preroll in the audio feeder instead).
    for encoder in ["libx264", "h264_nvenc"] {
        let args = build_ffmpeg_args(7001, 7002, encoder);
        assert!(
            !args.iter().any(|a| a == "-itsoffset"),
            "no -itsoffset argument for {encoder}"
        );
    }
}

#[test]
fn no_input_is_wall_clock_stamped_and_the_video_counts_the_output_rate() {
    // #221: ffmpeg's wall-clock stamps read the SYSTEM clock, which the box's
    // nightly UTC step moves (+1.54 s on 6.10.2026 02:00:11). With the video
    // input stamped and the PCM input counted, one step put the picture 3.5-4.7 s
    // behind the sound for the encoder's whole life (local repro, #221 comment
    // 6008217701), and a pause kept showing new pictures for over 6 s. So NO
    // input carries `-use_wallclock_as_timestamps`: the rawvideo input is
    // counted at `-framerate` (the feeder writes exactly that many frames per
    // second of the monotonic clock), equal to the output `-r`. The PCM input
    // keeps its sample count (#178 round 3: wall-clock-stamped PCM made the
    // box's ffmpeg mux ZERO audio packets).
    for encoder in ["libx264", "h264_nvenc"] {
        let args = build_ffmpeg_args(9001, 9002, encoder);
        assert!(
            !args.iter().any(|a| a == "-use_wallclock_as_timestamps"),
            "no wall-clock-stamped input for {encoder}"
        );
        let video_i = args
            .iter()
            .position(|a| a == "tcp://127.0.0.1:9001")
            .expect("video input URL present");
        let rates: Vec<usize> = args
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == "-framerate")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(rates.len(), 1, "exactly one counted input for {encoder}");
        assert_eq!(
            rates[0] + 3,
            video_i,
            "`-framerate N -i <video url>`: it is the video input's for {encoder}"
        );
        let out_rate = args
            .windows(2)
            .find(|w| w[0] == "-r")
            .map(|w| w[1].clone())
            .expect("an output -r");
        assert_eq!(args[rates[0] + 1], "25", "the video input counts 25 fps");
        assert_eq!(
            args[rates[0] + 1],
            out_rate,
            "the input rate is the output rate: no frame is dropped or repeated"
        );
    }
}

// ── #178 item 12: encoder restart budget ─────────────────────────────────────

#[test]
fn restart_budget_allows_three_per_rolling_minute_then_denies() {
    let mut b = RestartBudget::default();
    assert!(b.allow(0), "1st restart allowed");
    assert!(b.allow(1_000), "2nd allowed");
    assert!(b.allow(2_000), "3rd allowed");
    assert!(!b.allow(3_000), "4th within the minute is denied");
}

#[test]
fn restart_budget_evicts_after_the_window_exactly() {
    let mut b = RestartBudget::default();
    assert!(b.allow(0));
    assert!(b.allow(100));
    assert!(b.allow(200));
    // 59_999 ms after the first: still within the 60 s window (all 3 count) → denied.
    assert!(!b.allow(59_999));
    // Exactly 60_000 ms after the first: the first is evicted (>= window) → allowed.
    assert!(b.allow(60_000));
    // Now [100, 200, 60_000] are in-window → the next is denied.
    assert!(!b.allow(60_050));
}

#[test]
fn feeder_poll_keeps_the_written_audio_ahead_of_the_video() {
    // #184 round G3: the feeder pads silence once the written audio is
    // ALIGN_PAD_THRESHOLD_MS behind its write-ahead position, and only as often
    // as it polls. So the poll interval — plus a Windows timer oversleep of
    // ~15.6 ms — must stay under write-ahead − pad threshold (50 ms), or the
    // written audio drops behind the wall-clock video and ffmpeg waits for it.
    use crate::playback::preview::preview_audio_hold::AUDIO_WRITE_AHEAD_MS;
    use crate::playback::preview::preview_stream::ALIGN_PAD_THRESHOLD_MS;
    const TIMER_SLACK_US: u64 = 15_600;
    let margin_us = (AUDIO_WRITE_AHEAD_MS - ALIGN_PAD_THRESHOLD_MS) * 1_000;
    // black_box: a runtime value, not a constant assertion.
    let poll_us = std::hint::black_box(AFEED_POLL_US);
    assert_eq!(poll_us, 30_000);
    assert!(
        poll_us + TIMER_SLACK_US < margin_us,
        "poll {poll_us} µs + slack must stay under {margin_us} µs"
    );
}
