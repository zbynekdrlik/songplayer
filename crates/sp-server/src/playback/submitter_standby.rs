//! The pipeline outer loop's standby black (#147 standby same-path, design
//! comment 5841796900), split out of `submitter.rs` for the 1000-line cap.
//!
//! - [`FrameSubmitter::send_standby_black`] is what `run_loop_windows` calls at
//!   pipeline start, after a song ends / stops / errors, and on a Stop with no
//!   song. On the SDK-clocked (legacy) path it is the unchanged synchronous BGRA
//!   black. On the paced path it sends NOTHING: standby is the paced grid's job
//!   (the outer loop enters the idle fill at once, `pacer_sink::idle_poll`), so
//!   the paced output never carries a sync `send_video` or a BGRA frame.
//! - [`FrameSubmitter::standby_black_nv12`] is the ONE idle NV12 black the paced
//!   idle fill submits every boundary through the same #168 submit path as a
//!   playing frame, built once per pipeline instead of once per idle entry.

use sp_ndi::NdiBackend;

use super::FrameSubmitter;
use crate::playback::frame_buf::SharedFrame;

/// Neutral-black NV12 pixel bytes: Y = studio black 16, interleaved UV = 128.
fn black_nv12_bytes(width: u32, height: u32) -> Vec<u8> {
    let y = (width as usize) * (height as usize);
    let mut data = vec![16u8; y];
    data.resize(y + y / 2, 128u8);
    data
}

impl<B: NdiBackend> FrameSubmitter<B> {
    /// The outer loop's standby black. SDK-clocked: the synchronous BGRA black
    /// ([`send_black_bgra`](Self::send_black_bgra), unchanged). Paced: nothing —
    /// the paced idle fill emits the NV12 black + silence on the very next
    /// boundary through the same path as a playing frame (#147).
    pub fn send_standby_black(&mut self, width: u32, height: u32) {
        self.send_black_bgra(width, height);
    }

    /// The paced idle NV12 black for `width`×`height`, cached for the pipeline's
    /// life and handed out by `Arc` clone (a refcount bump, no pixel copy). A
    /// different size rebuilds it once.
    pub fn standby_black_nv12(&mut self, width: u32, height: u32) -> SharedFrame {
        if let Some((w, h, frame)) = &self.black_nv12
            && *w == width
            && *h == height
        {
            return frame.clone();
        }
        let frame = SharedFrame::new(black_nv12_bytes(width, height));
        self.black_nv12 = Some((width, height, frame.clone()));
        frame
    }
}
