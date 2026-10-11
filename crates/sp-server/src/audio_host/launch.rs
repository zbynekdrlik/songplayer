//! #233: the ASIO host's start, the parts both processes agree on (pure,
//! Linux-tested; the Windows glue is `client_win.rs` / `host_win.rs`).
//!
//! - The exe: `sp-audio-host.exe` NEXT TO SongPlayer's own exe (the
//!   installer ships it there: a second binary of the Tauri package, which
//!   the NSIS bundle installs beside the main one). A missing exe is a
//!   waiting reason that names the path, never a crash.
//! - The pipe: `\\.\pipe\songplayer-audio-host-<SongPlayer's pid>-<start>`,
//!   one per start (a new name for every restart, so a dying host's pipe
//!   never takes the next one), passed as `--pipe <name>`; SongPlayer makes
//!   the pipe (the only instance, local clients only) and the host opens it;
//!   a pipe opened by any other process than the host SongPlayer started is
//!   refused (#233 review).
//! - The host's own files: `%ProgramData%\SongPlayer` (the box's data dir)
//!   `sp-audio-host.log` and `sp-audio-host-panic.log`, else next to its exe.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The ASIO host's exe name.
pub const HOST_EXE: &str = "sp-audio-host.exe";
/// The host's log file in its data dir.
pub const HOST_LOG: &str = "sp-audio-host.log";
/// The host's crash record in its data dir (`panic_hook`).
pub const HOST_PANIC_LOG: &str = "sp-audio-host-panic.log";

/// The host's exe beside SongPlayer's exe `exe`.
pub fn host_exe_beside(exe: &Path) -> PathBuf {
    exe.with_file_name(HOST_EXE)
}

/// The waiting reason of a host exe that is not there.
pub fn missing_exe(path: &Path) -> String {
    format!(
        "{} is missing (the installer puts it next to SongPlayer.exe)",
        path.display()
    )
}

/// The pipe of SongPlayer `pid`'s `start`-th host.
pub fn pipe_name(pid: u32, start: u64) -> String {
    format!(r"\\.\pipe\songplayer-audio-host-{pid}-{start}")
}

/// The pipe name from the host's arguments (`--pipe <name>`).
pub fn pipe_arg(args: &[String]) -> Option<String> {
    args.iter()
        .position(|a| a == "--pipe")
        .and_then(|at| args.get(at + 1))
        .filter(|name| !name.is_empty())
        .cloned()
}

/// Whether the process that opened the host's pipe is the host SongPlayer
/// started (`expected`); `got` is its pid, `None` when Windows cannot tell.
/// Anything else is refused: the session never serves another process.
pub fn pipe_client_is_host(expected: u32, got: Option<u32>) -> Result<(), String> {
    match got {
        Some(pid) if pid == expected => Ok(()),
        Some(pid) => Err(format!(
            "process {pid} opened the host's pipe, not the host (process {expected}): refused"
        )),
        None => Err("the process that opened the host's pipe cannot be read: refused".into()),
    }
}

/// The host's data dir: `<ProgramData>\SongPlayer`, else `exe_dir`.
pub fn host_data_dir(program_data: Option<OsString>, exe_dir: &Path) -> PathBuf {
    match program_data {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("SongPlayer"),
        _ => exe_dir.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_host_exe_is_next_to_songplayers() {
        let exe = Path::new("Program Files")
            .join("SongPlayer")
            .join("SongPlayer.exe");
        let host = host_exe_beside(&exe);
        assert_eq!(host.parent(), exe.parent());
        assert_eq!(host.file_name().unwrap(), "sp-audio-host.exe");
        let text = missing_exe(&host);
        assert!(text.starts_with(&host.display().to_string()), "{text}");
        assert!(text.ends_with("is missing (the installer puts it next to SongPlayer.exe)"));
    }

    #[test]
    fn each_start_has_its_own_pipe() {
        assert_eq!(pipe_name(4321, 7), r"\\.\pipe\songplayer-audio-host-4321-7");
        assert_ne!(pipe_name(4321, 7), pipe_name(4321, 8));
    }

    #[test]
    fn the_pipe_comes_from_its_argument() {
        assert_eq!(
            pipe_arg(&args(&["--pipe", r"\\.\pipe\x"])),
            Some(r"\\.\pipe\x".into())
        );
        assert_eq!(
            pipe_arg(&args(&["--log", "a", "--pipe", "p"])),
            Some("p".into())
        );
        assert_eq!(pipe_arg(&args(&["--pipe"])), None);
        assert_eq!(pipe_arg(&args(&["--pipe", ""])), None);
        assert_eq!(pipe_arg(&args(&["p"])), None);
        assert_eq!(pipe_arg(&[]), None);
    }

    #[test]
    fn only_the_started_host_may_open_its_pipe() {
        assert_eq!(pipe_client_is_host(4321, Some(4321)), Ok(()));
        assert_eq!(
            pipe_client_is_host(4321, Some(77)),
            Err("process 77 opened the host's pipe, not the host (process 4321): refused".into())
        );
        assert_eq!(
            pipe_client_is_host(4321, None),
            Err("the process that opened the host's pipe cannot be read: refused".into())
        );
    }

    #[test]
    fn the_hosts_files_live_in_the_data_dir() {
        let exe_dir = Path::new("bin");
        assert_eq!(
            host_data_dir(Some(OsString::from("PD")), exe_dir),
            Path::new("PD").join("SongPlayer")
        );
        assert_eq!(host_data_dir(Some(OsString::new()), exe_dir), exe_dir);
        assert_eq!(host_data_dir(None, exe_dir), exe_dir);
    }
}
