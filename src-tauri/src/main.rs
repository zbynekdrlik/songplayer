//! SongPlayer Tauri desktop shell entry point.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    sp_tauri_lib::run();
}
