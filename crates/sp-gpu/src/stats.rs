//! What one composed frame and one Spout send cost: the telemetry S2
//! publishes as `max.upload_us_p99` / `max.draw_us_p99` /
//! `max.send_us_p99` (#223 R3-2).

/// The cost of one `Compositor::compose` call, measured on the calling
/// thread.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ComposeStats {
    /// The CPU time of the plane uploads (texture creation included), µs.
    pub upload_us: u64,
    /// From the first draw command until the GPU has finished the frame
    /// (the GPU side of the uploads included), µs.
    pub draw_us: u64,
    /// How many pictures this call uploaded: 0 (all resident), 1 or 2.
    pub uploads: u32,
}

/// The cost of one `SpoutSender::send` (#223 S1b), measured on the calling
/// thread.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpoutSendStats {
    /// The CPU time of Spout's `SendTexture`, µs: the wait for the sender's
    /// named mutex (Spout gives up after 67 ms), the queued copy into
    /// Spout's shared texture and the flush; at the first send also the
    /// shared texture's creation and the registration.
    pub send_us: u64,
}
