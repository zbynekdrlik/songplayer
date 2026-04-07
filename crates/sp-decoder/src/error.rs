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
    NoStream(&'static str),

    /// A sample read operation failed.
    #[error("Sample read failed: {0}")]
    ReadSample(String),

    /// The stream has reached its end.
    #[error("End of stream")]
    EndOfStream,

    /// A seek operation failed.
    #[error("Seek failed: {0}")]
    Seek(String),

    /// Locking the media buffer failed.
    #[error("Buffer lock failed: {0}")]
    BufferLock(String),
}
