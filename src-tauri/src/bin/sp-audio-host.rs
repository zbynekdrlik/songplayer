//! #233: `sp-audio-host.exe`, the ASIO host process. SongPlayer starts it
//! (next to `SongPlayer.exe`, in a kill-on-close Job object) and feeds it
//! over a local named pipe; it runs every ASIO output and spawns no
//! process, so no child SongPlayer starts can disturb an ASIO driver. All
//! of it lives in `sp_server::audio_host` (tested there); this is only the
//! entry point.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    std::process::exit(sp_server::audio_host::run_host_main());
}
