//! Whether a picture must be uploaded into its texture slot (pure).
//!
//! #223 R3-2: "a picture equal to the one already uploaded is not uploaded
//! again". Equal is the caller's id (`Nv12Picture::id`): a held, paused or
//! repeated picture (a 25 fps song on the 30 fps grid repeats every fifth
//! picture) costs no upload. A slot's textures have the size of the picture
//! last uploaded into it; another size needs new textures.

use crate::picture::Nv12Picture;

/// What a texture slot holds: the picture last uploaded into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resident {
    pub id: u64,
    pub width: u32,
    pub height: u32,
}

impl Resident {
    /// What a slot holds once `picture` is uploaded into it.
    pub fn of(picture: &Nv12Picture<'_>) -> Self {
        Self {
            id: picture.id,
            width: picture.width,
            height: picture.height,
        }
    }
}

/// What drawing a picture from a slot needs first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upload {
    /// The slot already holds this picture: nothing.
    Skip,
    /// The slot's textures have its size: write its planes into them.
    Write,
    /// No textures yet, or another size: create them, then write.
    Create,
}

/// What `picture` needs before it is drawn from a slot that holds
/// `resident`.
pub fn upload_for(resident: Option<Resident>, picture: &Nv12Picture<'_>) -> Upload {
    match resident {
        Some(held) if held.width == picture.width && held.height == picture.height => {
            if held.id == picture.id {
                Upload::Skip
            } else {
                Upload::Write
            }
        }
        _ => Upload::Create,
    }
}

#[cfg(test)]
#[path = "residency_tests.rs"]
mod tests;
