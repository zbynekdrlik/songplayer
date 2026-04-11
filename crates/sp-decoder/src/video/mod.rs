//! Windows Media Foundation video reader (video-only).
//!
//! This module is `cfg(windows)` because it depends on the
//! `windows::Win32::Media::MediaFoundation` bindings.

pub mod mf_reader;

pub use mf_reader::MediaFoundationVideoReader;
