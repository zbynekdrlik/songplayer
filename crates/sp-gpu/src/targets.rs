//! A compositor's render targets (#251 step 2), pure: which targets it
//! draws, under which id, and in which order.
//!
//! `SP-program-MAX` (3840×2160) and the FHD program `SP-program`
//! (1920×1080) are composed on ONE device from ONE upload of each picture
//! (#251, the FHD side's second device and second upload carried a large
//! part of the wall's residual jumps). So a compositor holds its PRIMARY
//! target, the one it was built with, for its whole life, and any number of
//! added ones; one compose draws every target, the primary first, then the
//! added ones in the order they were added. The Windows compositor keeps
//! its render targets in a [`Targets`] (`win/mod.rs` only calls Direct3D);
//! the book-keeping lives here so the Linux mutation gate sees it.

/// A target's id on its compositor. Ids are never reused: an id that named
/// a removed target names nothing, ever.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TargetId(u64);

impl TargetId {
    /// The target the compositor was built with (`SP-program-MAX`'s canvas
    /// in production), there for the compositor's whole life.
    pub const PRIMARY: TargetId = TargetId(0);
}

/// A compositor's targets, each with what the compositor keeps for it
/// (`T`: the Windows render target; a plain [`Target`](crate::Target) in
/// the tests), in draw order: the primary first, then the added ones in the
/// order they were added.
#[derive(Debug)]
pub struct Targets<T> {
    entries: Vec<(TargetId, T)>,
    /// The id the next added target gets.
    next: u64,
}

impl<T> Targets<T> {
    /// The primary target alone.
    pub fn new(primary: T) -> Self {
        Self {
            entries: vec![(TargetId::PRIMARY, primary)],
            next: 1,
        }
    }

    /// Add a target, drawn after every target already there; its new id.
    pub fn add(&mut self, target: T) -> TargetId {
        let id = TargetId(self.next);
        self.next += 1;
        self.entries.push((id, target));
        id
    }

    /// Take an added target out (`None`: no such target). The primary is
    /// the compositor's for its life: it is never removed.
    pub fn remove(&mut self, id: TargetId) -> Option<T> {
        let at = self
            .entries
            .iter()
            .skip(1)
            .position(|(held, _)| *held == id)?;
        Some(self.entries.remove(at + 1).1)
    }

    /// The target `id` names, if the compositor holds it.
    pub fn get(&self, id: TargetId) -> Option<&T> {
        self.entries
            .iter()
            .find(|(held, _)| *held == id)
            .map(|(_, target)| target)
    }

    /// The target `id` names, if the compositor holds it.
    pub fn get_mut(&mut self, id: TargetId) -> Option<&mut T> {
        self.entries
            .iter_mut()
            .find(|(held, _)| *held == id)
            .map(|(_, target)| target)
    }

    /// The primary target.
    pub fn primary(&self) -> &T {
        &self.entries[0].1
    }

    /// Every target with its id, in draw order.
    pub fn iter(&self) -> impl Iterator<Item = (TargetId, &T)> {
        self.entries.iter().map(|(id, target)| (*id, target))
    }

    /// Every target with its id, in draw order.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (TargetId, &mut T)> {
        self.entries.iter_mut().map(|(id, target)| (*id, target))
    }
}

#[cfg(test)]
#[path = "targets_tests.rs"]
mod tests;
