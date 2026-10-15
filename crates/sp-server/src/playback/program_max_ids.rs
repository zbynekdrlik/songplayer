//! #223 S2: the ids of the pictures the `program-max` thread gives its
//! compositor (moved out of `program_max_worker.rs`, #251 step 2, which sits
//! at the 1000-line cap). Pure: tested in `program_max_worker_tests.rs`.

use sp_gpu::{Composition, Nv12Picture};

use crate::playback::frame_buf::SharedFrame;
use crate::playback::program_max::{MaxJob, MaxPicture};

/// The ids of the pictures the compositor is given. `sp-gpu` skips the
/// upload of a picture whose id its slot already holds, so an id must never
/// name other bytes: ids come from a counter, never from an address (a
/// freed buffer's address comes back). A picture that is the SAME
/// allocation as one of the last composed boundary's keeps that one's id;
/// those allocations are held here, so none of them can be freed and
/// reused while it is compared.
#[derive(Debug, Default)]
pub struct PictureIds {
    next: u64,
    held: Vec<(SharedFrame, u64)>,
}

impl PictureIds {
    /// What `job` shows, its pictures labelled; this job's pictures are the
    /// ones held from now on.
    pub fn composition<'a>(&mut self, job: &'a MaxJob) -> Composition<'a> {
        let mut labelled = Vec::with_capacity(2);
        let composition = match job {
            MaxJob::Black { .. } => Composition::Black,
            MaxJob::Picture { picture, .. } => {
                Composition::Picture(self.label(&mut labelled, picture))
            }
            MaxJob::Fade {
                from,
                to,
                weight_q8,
                ..
            } => Composition::Fade {
                from: from.as_ref().map(|p| self.label(&mut labelled, p)),
                to: to.as_ref().map(|p| self.label(&mut labelled, p)),
                weight_q8: *weight_q8,
            },
        };
        self.held = labelled;
        composition
    }

    /// Hold no picture any more (the compositor is gone): the frames are
    /// freed, and every picture gets a new id.
    pub fn forget(&mut self) {
        self.held.clear();
    }

    /// `picture` with its id: the id of the same allocation held or already
    /// labelled in this job, else the next one.
    fn label<'a>(
        &mut self,
        labelled: &mut Vec<(SharedFrame, u64)>,
        picture: &'a MaxPicture,
    ) -> Nv12Picture<'a> {
        let known = self
            .held
            .iter()
            .chain(labelled.iter())
            .find(|(frame, _)| frame.ptr_eq(&picture.video))
            .map(|&(_, id)| id);
        let id = match known {
            Some(id) => id,
            None => {
                self.next += 1;
                self.next
            }
        };
        labelled.push((picture.video.clone(), id));
        picture.nv12(id)
    }
}
