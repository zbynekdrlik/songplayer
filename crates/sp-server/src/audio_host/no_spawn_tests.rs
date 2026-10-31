//! #233: the ASIO host spawns NO process — the reason it exists (a child
//! spawned from the process that holds a Waves SoundGrid driver resyncs
//! that driver; measured on PP). A source scan of the host's code path,
//! like the Test Integrity job's "never sets the driver's rate" scan: the
//! host's entry, its frame loop and glue, and the ASIO output it runs (the
//! worker, the driver glue, the servo, the resampler, the queue, the clocks)
//! must name no way to start a process. SongPlayer's own side
//! (`client_win.rs`, which starts the host) is deliberately not listed.

/// The host's code path, by file (comments are not scanned).
const HOST_PATH: [(&str, &str); 40] = [
    (
        "src-tauri/src/bin/sp-audio-host.rs",
        include_str!("../../../../src-tauri/src/bin/sp-audio-host.rs"),
    ),
    ("audio_host/mod.rs", include_str!("mod.rs")),
    ("audio_host/host.rs", include_str!("host.rs")),
    ("audio_host/host_win.rs", include_str!("host_win.rs")),
    ("audio_host/protocol.rs", include_str!("protocol.rs")),
    ("audio_host/wire_status.rs", include_str!("wire_status.rs")),
    (
        "audio_host/remote_clock.rs",
        include_str!("remote_clock.rs"),
    ),
    ("audio_host/mono.rs", include_str!("mono.rs")),
    ("audio_host/log_forward.rs", include_str!("log_forward.rs")),
    ("audio_host/launch.rs", include_str!("launch.rs")),
    ("panic_hook.rs", include_str!("../panic_hook.rs")),
    ("log_rotate.rs", include_str!("../log_rotate.rs")),
    (
        "playback/asio_out.rs",
        include_str!("../playback/asio_out.rs"),
    ),
    (
        "playback/asio_out_events.rs",
        include_str!("../playback/asio_out_events.rs"),
    ),
    (
        "playback/asio_out_remote.rs",
        include_str!("../playback/asio_out_remote.rs"),
    ),
    (
        "playback/asio_out_loop.rs",
        include_str!("../playback/asio_out_loop.rs"),
    ),
    (
        "playback/asio_out_status.rs",
        include_str!("../playback/asio_out_status.rs"),
    ),
    (
        "playback/asio_loop.rs",
        include_str!("../playback/asio_loop.rs"),
    ),
    (
        "playback/asio_loop_search.rs",
        include_str!("../playback/asio_loop_search.rs"),
    ),
    (
        "playback/asio_win.rs",
        include_str!("../playback/asio_win.rs"),
    ),
    (
        "playback/asio_state.rs",
        include_str!("../playback/asio_state.rs"),
    ),
    (
        "playback/asio_format.rs",
        include_str!("../playback/asio_format.rs"),
    ),
    (
        "playback/asio_hold.rs",
        include_str!("../playback/asio_hold.rs"),
    ),
    ("playback/asrc.rs", include_str!("../playback/asrc.rs")),
    (
        "playback/asrc_servo.rs",
        include_str!("../playback/asrc_servo.rs"),
    ),
    (
        "playback/audio_out_queue.rs",
        include_str!("../playback/audio_out_queue.rs"),
    ),
    (
        "playback/audio_out_block.rs",
        include_str!("../playback/audio_out_block.rs"),
    ),
    (
        "playback/vban_clock.rs",
        include_str!("../playback/vban_clock.rs"),
    ),
    (
        "playback/wallclock.rs",
        include_str!("../playback/wallclock.rs"),
    ),
    (
        "playback/wallclock_anchor.rs",
        include_str!("../playback/wallclock_anchor.rs"),
    ),
    (
        "playback/fleet_shift.rs",
        include_str!("../playback/fleet_shift.rs"),
    ),
    (
        "playback/pipeline_paced.rs",
        include_str!("../playback/pipeline_paced.rs"),
    ),
    (
        "playback/paced_producer.rs",
        include_str!("../playback/paced_producer.rs"),
    ),
    (
        "playback/vban_out.rs",
        include_str!("../playback/vban_out.rs"),
    ),
    (
        "playback/vban_packet.rs",
        include_str!("../playback/vban_packet.rs"),
    ),
    (
        "playback/audio_out.rs",
        include_str!("../playback/audio_out.rs"),
    ),
    (
        "playback/program_output.rs",
        include_str!("../playback/program_output.rs"),
    ),
    ("playback/mmcss.rs", include_str!("../playback/mmcss.rs")),
    (
        "playback/stat_window.rs",
        include_str!("../playback/stat_window.rs"),
    ),
    (
        "sp-asrc/src/lib.rs",
        include_str!("../../../sp-asrc/src/lib.rs"),
    ),
];

/// Every way to start a process from Rust or Win32 (word starts, so a
/// longer name such as `CreateProcessW` counts too).
const SPAWNS: [&str; 7] = [
    "Command::new",
    "process::Command",
    "tokio::process",
    "CreateProcess",
    "ShellExecute",
    "WinExec",
    "posix_spawn",
];

/// `text` without its `//` comments (doc lines included).
fn code_of(text: &str) -> String {
    text.lines()
        .map(|line| line.split("//").next().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The spawns `text`'s code names, as `file:line: needle`.
fn spawns_in(file: &str, text: &str) -> Vec<String> {
    code_of(text)
        .lines()
        .enumerate()
        .flat_map(|(i, line)| {
            SPAWNS
                .iter()
                .filter(move |needle| line.contains(*needle))
                .map(move |needle| format!("{file}:{}: {needle}", i + 1))
        })
        .collect()
}

#[test]
fn the_asio_host_spawns_no_process() {
    let found: Vec<String> = HOST_PATH
        .iter()
        .flat_map(|(file, text)| spawns_in(file, text))
        .collect();
    assert!(
        found.is_empty(),
        "the ASIO host must spawn no process: {found:?}"
    );
}

/// The scan sees a spawn in code, at its line, and ignores one in a comment
/// (so it can never pass by reading nothing).
#[test]
fn the_scan_finds_a_spawn_in_code_only() {
    let text = "fn a() {}\n// Command::new(\"x\") in a comment\nlet c = std::process::Command::new(\"yt-dlp\");\n";
    assert_eq!(
        spawns_in("x.rs", text),
        ["x.rs:3: Command::new", "x.rs:3: process::Command"]
    );
    assert_eq!(
        spawns_in("y.rs", "unsafe { CreateProcessW(a) }"),
        ["y.rs:1: CreateProcess"]
    );
    assert!(spawns_in("z.rs", "/// tokio::process::Child\nfn z() {}").is_empty());
    // The host's own files are really read.
    assert!(HOST_PATH.iter().all(|(_, text)| text.len() > 100));
    assert!(code_of(HOST_PATH[2].1).contains("pub async fn serve"));
}
