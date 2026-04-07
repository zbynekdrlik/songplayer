//! Error types for NDI operations.

use std::fmt;

/// Errors that can occur when loading or using the NDI SDK.
#[derive(Debug)]
pub enum NdiError {
    /// NDI runtime library could not be found.
    LibraryNotFound(String),
    /// A required symbol was missing from the NDI library.
    SymbolNotFound(String),
    /// `NDIlib_initialize()` returned false.
    InitFailed,
}

impl fmt::Display for NdiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NdiError::LibraryNotFound(msg) => write!(f, "NDI library not found: {msg}"),
            NdiError::SymbolNotFound(sym) => write!(f, "NDI symbol not found: {sym}"),
            NdiError::InitFailed => write!(f, "NDIlib_initialize() returned false"),
        }
    }
}

impl std::error::Error for NdiError {}
