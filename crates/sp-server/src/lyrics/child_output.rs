//! #171 — shared helpers for draining and tailing a heavy Python child's
//! stderr/stdout so a failure's traceback reaches the log.
//!
//! The isolation child (`aligner::preprocess_vocals`) and the stem-separation
//! child (`stems::separator::separate_stems`) both spawn a Python subprocess
//! bounded by the stall waiter (`heavy_plan::wait_with_stall_timeout`, which
//! only calls `child.wait()`), so the stdio pipes must be drained concurrently
//! or they deadlock the child — and on a failure the drained tail is the ONLY
//! place the Python traceback survives (the child no longer inherits the
//! parent's stdio). The pure formatting (`tail_lines` / `failure_tail`) is
//! Linux-unit-tested; `drain_pipe` is thin I/O.

/// Minimum trimmed-stderr length (in chars) at which stderr is preferred over
/// stdout as the failure-tail source. `1` = any non-empty stderr wins (Python
/// writes its error + `{"error": ...}` to stderr); a fully-empty stderr falls
/// back to stdout. Isolated as a constant so the RED test on the no-compile box
/// can flip it (RED ships a huge sentinel → the "prefers stderr" test fails,
/// GREEN sets `1`).
pub(crate) const STDERR_PREFER_MIN_CHARS: usize = 1;

/// The best failure tail for a heavy child: the last `n` lines of stderr when it
/// carries any content (the Python traceback lands there), else stdout. Pure —
/// unit-tested.
pub(crate) fn failure_tail(stderr: &str, stdout: &str, n: usize, max_len: usize) -> String {
    let src = if stderr.trim().chars().count() >= STDERR_PREFER_MIN_CHARS {
        stderr
    } else {
        stdout
    };
    tail_lines(src, n, max_len)
}

/// Last `n` lines of `s`, each truncated to `max_len` chars (append `…` when
/// truncated), joined with `\n`. Pure — unit-tested.
pub(crate) fn tail_lines(s: &str, n: usize, max_len: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..]
        .iter()
        .map(|line| {
            if line.chars().count() > max_len {
                let truncated: String = line.chars().take(max_len).collect();
                format!("{truncated}…")
            } else {
                (*line).to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Spawn a task that reads `pipe` to EOF into a byte buffer, returning its
/// handle. Draining concurrently with the stall waiter keeps the child's stdio
/// pipes from filling and deadlocking it. I/O — `mutants::skip`.
#[cfg_attr(test, mutants::skip)]
pub(crate) fn drain_pipe<R>(mut pipe: R) -> tokio::task::JoinHandle<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf).await;
        buf
    })
}

#[cfg(test)]
mod tests {
    use super::{failure_tail, tail_lines};

    #[test]
    fn keeps_only_the_last_n_lines() {
        let input = (1..=25)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let out = tail_lines(&input, 20, 300);
        let out_lines: Vec<&str> = out.lines().collect();
        assert_eq!(out_lines.len(), 20);
        assert_eq!(out_lines.first(), Some(&"6"));
        assert_eq!(out_lines.last(), Some(&"25"));
    }

    #[test]
    fn truncates_a_long_line() {
        let long = "x".repeat(500);
        let out = tail_lines(&long, 20, 300);
        assert_eq!(out.chars().count(), 301); // 300 + the ellipsis
        assert!(out.ends_with('…'));
    }

    #[test]
    fn empty_in_empty_out() {
        assert_eq!(tail_lines("", 20, 300), "");
    }

    #[test]
    fn truncation_boundary_keeps_a_line_exactly_max_len() {
        // The truncation test is `count > max_len`, NOT `>= max_len`: a line of
        // EXACTLY max_len chars is kept verbatim; only a longer line is cut.
        // (Kills the child_output.rs:41 `>`→`>=` mutant, which would truncate an
        // exact-length line.)
        let exact = "y".repeat(50);
        let out = tail_lines(&exact, 20, 50);
        assert_eq!(
            out, exact,
            "a line of exactly max_len must not be truncated"
        );
        assert!(!out.ends_with('…'));

        let over = "y".repeat(51);
        let out2 = tail_lines(&over, 20, 50);
        assert_eq!(out2.chars().count(), 51, "max_len + the ellipsis");
        assert!(out2.ends_with('…'));
    }

    #[test]
    fn keeps_exactly_n_lines_then_drops_the_oldest() {
        // Line-count boundary of `saturating_sub(n)`: exactly n lines are all
        // kept; the (n+1)-th input drops the OLDEST.
        let five = (1..=5)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(tail_lines(&five, 5, 300), five, "exactly n lines: all kept");

        let six = (1..=6)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let out = tail_lines(&six, 5, 300);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 5);
        assert_eq!(lines.first(), Some(&"2"), "oldest line dropped");
        assert_eq!(lines.last(), Some(&"6"));
    }

    #[test]
    fn failure_tail_prefers_stderr_when_nonempty() {
        // The Python child writes its traceback to stderr; the tail MUST surface
        // it even when stdout also has content. (RED fails here:
        // STDERR_PREFER_MIN_CHARS starts at 1_000_000_000, so stdout is used.)
        let stderr = "  \nTraceback (most recent call last):\nRuntimeError: dereverb boom";
        let stdout = "progress: 100%";
        let tail = failure_tail(stderr, stdout, 30, 300);
        assert!(
            tail.contains("RuntimeError: dereverb boom"),
            "failure tail must come from stderr, got: {tail}"
        );
        assert!(!tail.contains("progress: 100%"));
    }

    #[test]
    fn failure_tail_falls_back_to_stdout_when_stderr_blank() {
        // A child that printed only to stdout (no stderr) still yields a tail.
        let tail = failure_tail("   \n\n  ", "only-on-stdout error detail", 30, 300);
        assert!(tail.contains("only-on-stdout error detail"));
    }
}
