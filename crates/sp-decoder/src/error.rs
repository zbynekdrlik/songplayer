//! Decoder error types.

/// Errors that can occur during media decoding.
#[derive(Debug, thiserror::Error)]
pub enum DecoderError {
    /// COM initialization failed.
    #[error("COM initialization failed: {0}")]
    ComInit(String),

    /// Failed to create the MF source reader.
    #[error("Failed to create source reader: {0}")]
    SourceReader(String),

    /// No stream of the given kind is available.
    #[error("No {0} stream available")]
    NoStream(String),

    /// A sample read operation failed.
    #[error("Sample read failed: {0}")]
    ReadSample(String),

    /// A seek operation failed.
    #[error("Seek failed: {0}")]
    Seek(String),

    /// Locking the media buffer failed.
    #[error("Buffer lock failed: {0}")]
    BufferLock(String),

    /// I/O failure opening or reading a file.
    #[error("I/O failure: {0}")]
    Io(String),

    /// Decoder-side failure (Symphonia or MF codec error).
    #[error("Decode failure: {0}")]
    Decode(String),

    /// Video and audio sidecars disagree on duration / format.
    #[error("Video/audio mismatch: {0}")]
    Mismatch(String),

    /// #207: a per-frame video buffer could not be allocated (host out of
    /// commit). The pipeline maps this to a dropped frame + a rate-limited WARN +
    /// a `frames_dropped_alloc` counter, so the wall stutters instead of the
    /// process aborting (`handle_alloc_error`, the #156 `0xc0000409` class).
    #[error("Frame buffer allocation failed: {0} bytes")]
    FrameAlloc(usize),
}
