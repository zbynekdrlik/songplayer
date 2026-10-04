//! What one composed frame cost: the telemetry S2 publishes as
//! `max.upload_us_p99` / `max.draw_us_p99` (#223 R3-2).

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
