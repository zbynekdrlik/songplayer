//! #233: the ASIO host process, `sp-audio-host.exe`.
//!
//! **Why.** Measured on PP (11.10.2026): every child process SongPlayer
//! spawns (yt-dlp for a playlist sync or a probe, ffmpeg, the python
//! workers) made the Waves SoundGrid ASIO driver — loaded in SongPlayer's own
//! process by its ASIO output — ask for a resync, and the output popped;
//! the same yt-dlp spawned from a shell never did. SongPlayer must spawn
//! children, so no ASIO driver lives in SongPlayer any more: EVERY ASIO
//! output runs in this host process, which spawns nothing (a source scan in
//! CI, `no_spawn_tests.rs`), whatever the in-process mechanism was.
//!
//! **Shape.** The host links this crate and runs the existing ASIO output
//! unchanged — `AsioWorker`, the `asio_win` device, the servo, the
//! resampler, the splice, the ring; only the block source (the pipe) and
//! the status sink (the pipe) are new:
//!
//! - `protocol.rs`: the pipe's length-prefixed frames (typed JSON control
//!   and reports, binary blocks), bounded before anything is allocated;
//! - `wire_status.rs`: an output's status on the pipe;
//! - `remote_clock.rs` + `mono.rs`: SongPlayer's timeline read in the host
//!   (SongPlayer's anchors on the box's one monotonic counter: the host has
//!   no wall, so it never relabels a date step on its own);
//! - `host.rs`: the host's frame loop; `log_forward.rs`: its log lines to
//!   SongPlayer;
//! - `client.rs`: SongPlayer's side (each ASIO output's forwarder, the
//!   session, the supervisor); `restart.rs`: the restart backoff;
//! - `launch.rs`: the exe, the pipe's name, the host's files;
//! - `client_win.rs` / `host_win.rs`: the Windows glue (process + Job object,
//!   pipe, the link thread, priority), out of the mutation gate.
//!
//! **Lifecycle.** SongPlayer starts the host when its first ASIO output is
//! built, in a Job object that kills it with SongPlayer; the host exits at
//! the pipe's end. A host that is missing, does not start or ends is
//! restarted after 2 / 10 / 30 / 60 s, its outputs reading waiting,
//! `host_down`, meanwhile. VBAN outputs stay in SongPlayer (no driver).

pub mod client;
#[cfg(windows)]
pub mod client_win;
pub mod host;
#[cfg(windows)]
pub mod host_win;
pub mod launch;
pub mod log_forward;
pub mod mono;
pub mod protocol;
pub mod remote_clock;
pub mod restart;
pub mod wire_status;

/// The ASIO host's `main` (`src-tauri/src/bin/sp-audio-host.rs`): the exit
/// code. `--pipe <name>` names SongPlayer's pipe.
#[cfg_attr(test, mutants::skip)] // the process entry; its argument parse is `launch::pipe_arg` (tested)
pub fn run_host_main() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(pipe) = launch::pipe_arg(&args) else {
        eprintln!("usage: sp-audio-host --pipe <name> (SongPlayer starts it)");
        return 2;
    };
    #[cfg(windows)]
    {
        host_win::run(&pipe)
    }
    #[cfg(not(windows))]
    {
        eprintln!("sp-audio-host runs on Windows only (pipe {pipe})");
        2
    }
}

#[cfg(test)]
mod host_tests;
#[cfg(test)]
mod no_spawn_tests;
