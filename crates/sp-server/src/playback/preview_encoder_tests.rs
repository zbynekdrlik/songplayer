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
    // lead_ms 0 (paced path) → no -itsoffset; the full vector is unchanged.
    let args = build_ffmpeg_args(5001, 5002, "libx264", 0);
    let expected: Vec<String> = [
        "-hide_banner",
        "-loglevel",
        "error",
        "-use_wallclock_as_timestamps",
        "1",
        "-f",
        "rawvideo",
        "-pix_fmt",
        "nv12",
        "-s",
        "640x360",
        "-i",
        "tcp://127.0.0.1:5001",
        "-use_wallclock_as_timestamps",
        "1",
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
        "1200k",
        "-g",
        "25",
        "-fps_mode",
        "cfr",
        "-r",
        "25",
        "-c:a",
        "aac",
        "-b:a",
        "128k",
        "-movflags",
        "+frag_keyframe+empty_moov+default_base_moof",
        "-frag_duration",
        "500000",
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
fn ffmpeg_args_omit_libx264_tuning_for_hardware_encoders() {
    let args = build_ffmpeg_args(1, 2, "h264_nvenc", 0);
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
fn ffmpeg_args_insert_itsoffset_before_audio_input_for_nonzero_lead() {
    // SDK-clocked path: the #192 lookahead makes tapped audio LEAD video by
    // 100 ms, compensated by `-itsoffset 0.100` placed BEFORE the audio input
    // (delaying audio back into sync). Exact-boundary: value is 0.100, exactly
    // one occurrence, sitting after the video `-i` and before the audio input.
    let args = build_ffmpeg_args(7001, 7002, "libx264", 100);
    let off = args
        .iter()
        .position(|a| a == "-itsoffset")
        .expect("lead 100 ms → -itsoffset present");
    assert_eq!(args[off + 1], "0.100", "100 ms lead → 0.100 s offset");
    assert_eq!(
        args.iter().filter(|a| *a == "-itsoffset").count(),
        1,
        "exactly one -itsoffset (only the audio input)"
    );
    let video_i = args
        .iter()
        .position(|a| a == "tcp://127.0.0.1:7001")
        .expect("video input URL present");
    let audio_f = args
        .iter()
        .position(|a| a == "f32le")
        .expect("audio -f f32le present");
    let audio_i = args
        .iter()
        .position(|a| a == "tcp://127.0.0.1:7002")
        .expect("audio input URL present");
    assert!(video_i < off, "-itsoffset comes AFTER the video input");
    assert!(off < audio_f, "-itsoffset comes BEFORE the audio -f f32le");
    assert!(audio_f < audio_i, "audio format precedes its -i");
}

#[test]
fn ffmpeg_args_omit_itsoffset_for_zero_lead() {
    // Paced path (genlock_pacing=true): lead 0 → no compensation flag at all.
    let args = build_ffmpeg_args(1, 2, "libx264", 0);
    assert!(
        !args.iter().any(|a| a == "-itsoffset"),
        "lead 0 → no -itsoffset"
    );
}
