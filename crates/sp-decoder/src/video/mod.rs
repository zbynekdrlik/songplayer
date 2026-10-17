//! Windows Media Foundation video reader (video-only).
//!
//! This module is `cfg(windows)` because it depends on the
//! `windows::Win32::Media::MediaFoundation` bindings. #223 S3b: the reader's
//! opt-in hardware decode (`hw_session.rs`: the Direct3D 11 device and its
//! DXGI device manager; `dxgi_frame.rs`: a GPU-decoded picture copied back)
//! takes every decision from `crate::hw_decode`. #282: the decode cap's
//! scaled open is `mf_reader_scaled.rs` (a child module of `mf_reader.rs`)
//! and `mf_scaled.rs` (its Media Foundation reads).

mod dxgi_frame;
mod hw_session;
pub mod mf_reader;
mod mf_scaled; // #282: the scaled open's MF reads (type, chain, facts, refusal guard)

#[doc(hidden)]
pub use dxgi_frame::{DxgiPicture, read_texture_as_decoded_sample};
pub use mf_reader::MediaFoundationVideoReader;
// #282 follow-up: what a scaled open read of Media Foundation, and the
// tests' refusal guard (`MediaFoundationVideoReader::scaled_open_facts`,
// `refuse_next_scaled_open_for_test`).
#[doc(hidden)]
pub use mf_scaled::{ChainStage, Colour, DecoderOutput, ScaledOpenFacts, ScaledOpenRefusal};
