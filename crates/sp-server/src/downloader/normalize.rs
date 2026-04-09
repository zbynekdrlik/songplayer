//! FFmpeg 2-pass loudnorm audio normalization (-14 LUFS).

use std::path::Path;

use super::hide_console_window;

/// Statistics extracted from FFmpeg's first-pass loudnorm analysis.
#[derive(Debug, Clone)]
struct LoudnormStats {
    input_i: String,
    input_tp: String,
    input_lra: String,
    input_thresh: String,
    target_offset: String,
}

/// Normalize audio to -14 LUFS using FFmpeg 2-pass loudnorm.
///
/// Video stream is copied (`-c:v copy`), audio re-encoded to AAC 192k.
pub async fn normalize_audio(
    ffmpeg: &Path,
    input: &Path,
    output: &Path,
) -> Result<(), anyhow::Error> {
    // Pass 1: measure loudness stats.
    let mut cmd1 = tokio::process::Command::new(ffmpeg);
    cmd1.args(["-i"])
        .arg(input)
        .args([
            "-af",
            "loudnorm=I=-14:TP=-1:LRA=11:print_format=json",
            "-f",
            "null",
        ])
        .arg(null_output())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    hide_console_window(&mut cmd1);
    let pass1 = cmd1.output().await?;

    if !pass1.status.success() {
        let stderr = String::from_utf8_lossy(&pass1.stderr);
        anyhow::bail!("ffmpeg pass 1 failed: {stderr}");
    }

    let stderr = String::from_utf8_lossy(&pass1.stderr);
    let stats = extract_loudnorm_stats(&stderr)
        .ok_or_else(|| anyhow::anyhow!("failed to parse loudnorm stats from ffmpeg output"))?;

    // Pass 2: apply measured values.
    let af_filter = format!(
        "loudnorm=I=-14:TP=-1:LRA=11:\
         measured_I={}:measured_TP={}:measured_LRA={}:\
         measured_thresh={}:offset={}",
        stats.input_i, stats.input_tp, stats.input_lra, stats.input_thresh, stats.target_offset,
    );

    let mut cmd2 = tokio::process::Command::new(ffmpeg);
    cmd2.args(["-i"])
        .arg(input)
        .args(["-af", &af_filter])
        .args(["-c:v", "copy", "-c:a", "aac", "-b:a", "192k", "-y"])
        .arg(output)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    hide_console_window(&mut cmd2);
    let pass2 = cmd2.output().await?;

    if !pass2.status.success() {
        let stderr = String::from_utf8_lossy(&pass2.stderr);
        anyhow::bail!("ffmpeg pass 2 failed: {stderr}");
    }

    tracing::info!("normalized {} -> {}", input.display(), output.display());
    Ok(())
}

/// Extract loudnorm statistics JSON from FFmpeg stderr output.
///
/// FFmpeg prints a JSON block at the end of pass 1 containing the measured
/// loudness values.  We find the last `{…}` block that contains `"input_i"`.
fn extract_loudnorm_stats(stderr: &str) -> Option<LoudnormStats> {
    // Find the last JSON object in the output that contains loudnorm data.
    // Handle both Unix (\n) and Windows (\r\n) line endings.
    let json_start = stderr.rfind("{\r\n").or_else(|| stderr.rfind("{\n"))?;
    let json_end = stderr[json_start..].find('}')? + json_start + 1;
    let json_str = &stderr[json_start..json_end];

    // Parse with serde_json.
    let obj: serde_json::Value = serde_json::from_str(json_str).ok()?;

    Some(LoudnormStats {
        input_i: obj.get("input_i")?.as_str()?.to_string(),
        input_tp: obj.get("input_tp")?.as_str()?.to_string(),
        input_lra: obj.get("input_lra")?.as_str()?.to_string(),
        input_thresh: obj.get("input_thresh")?.as_str()?.to_string(),
        target_offset: obj.get("target_offset")?.as_str()?.to_string(),
    })
}

/// Platform-appropriate null output device.
fn null_output() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_FFMPEG_OUTPUT: &str = r#"
[Parsed_loudnorm_0 @ 0x562e9a5c0d80]
{
	"input_i" : "-24.12",
	"input_tp" : "-3.45",
	"input_lra" : "7.80",
	"input_thresh" : "-34.56",
	"output_i" : "-14.00",
	"output_tp" : "-1.00",
	"output_lra" : "6.50",
	"output_thresh" : "-24.44",
	"normalization_type" : "dynamic",
	"target_offset" : "0.12"
}
"#;

    #[test]
    fn parse_loudnorm_stats_from_real_output() {
        let stats = extract_loudnorm_stats(SAMPLE_FFMPEG_OUTPUT).unwrap();
        assert_eq!(stats.input_i, "-24.12");
        assert_eq!(stats.input_tp, "-3.45");
        assert_eq!(stats.input_lra, "7.80");
        assert_eq!(stats.input_thresh, "-34.56");
        assert_eq!(stats.target_offset, "0.12");
    }

    #[test]
    fn parse_loudnorm_stats_missing_field() {
        let bad_json = r#"some ffmpeg output
{
    "input_i" : "-24.12",
    "input_tp" : "-3.45"
}
"#;
        // Missing required fields → None.
        assert!(extract_loudnorm_stats(bad_json).is_none());
    }

    #[test]
    fn parse_loudnorm_stats_no_json() {
        assert!(extract_loudnorm_stats("no json here at all").is_none());
    }

    #[test]
    fn parse_loudnorm_stats_windows_line_endings() {
        let win_output = "[Parsed_loudnorm_0 @ 0x562e9a5c0d80]\r\n{\r\n\t\"input_i\" : \"-24.12\",\r\n\t\"input_tp\" : \"-3.45\",\r\n\t\"input_lra\" : \"7.80\",\r\n\t\"input_thresh\" : \"-34.56\",\r\n\t\"output_i\" : \"-14.00\",\r\n\t\"output_tp\" : \"-1.00\",\r\n\t\"output_lra\" : \"6.50\",\r\n\t\"output_thresh\" : \"-24.44\",\r\n\t\"normalization_type\" : \"dynamic\",\r\n\t\"target_offset\" : \"0.12\"\r\n}\r\n";
        let stats = extract_loudnorm_stats(win_output).unwrap();
        assert_eq!(stats.input_i, "-24.12");
        assert_eq!(stats.target_offset, "0.12");
    }

    #[test]
    fn null_output_is_valid() {
        let dev = null_output();
        assert!(!dev.is_empty());
    }
}
