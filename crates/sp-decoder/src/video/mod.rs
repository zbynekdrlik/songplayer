//! Windows Media Foundation video reader (video-only).
//!
//! This module is `cfg(windows)` because it depends on the
//! `windows::Win32::Media::MediaFoundation` bindings. #223 S3b: the reader's
//! opt-in hardware decode (`hw_session.rs`: the Direct3D 11 device and its
//! DXGI device manager; `dxgi_frame.rs`: a GPU-decoded picture copied back)
//! takes every decision from `crate::hw_decode`.

mod dxgi_frame;
mod hw_session;
pub mod mf_reader;

pub use mf_reader::MediaFoundationVideoReader;
