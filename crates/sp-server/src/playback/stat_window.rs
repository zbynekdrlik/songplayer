//! #210 part 2 (review round 1): the two pure helpers the `SP-program`
//! sender's boundary timing (`program_output_timing.rs`) and the VBAN
//! thread's late packets (`vban_stall.rs`) share, instead of each keeping a
//! copy:
//!
//! - [`TwoBucketWorst`]: the worst sample of the bucket being filled and of
//!   the last full one, `LEN` samples per bucket (a const parameter, so one
//!   window never mixes lengths), so a figure covers the last one to two
//!   buckets (60–120 s for a 60 s bucket);
//! - [`WarnLimiter`]: at most one WARN per period of a timeline, counting
//!   the ones it held back for the next WARN to report.

/// A sample with a "worst of two": each figure's max.
pub trait Worst: Copy + Default {
    fn worst(self, other: Self) -> Self;
}

impl Worst for u64 {
    fn worst(self, other: Self) -> Self {
        self.max(other)
    }
}

/// The worst sample of the last one to two buckets of `LEN` samples.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TwoBucketWorst<T, const LEN: u32> {
    /// The worst of the bucket being filled, and of the last full one.
    current: T,
    previous: T,
    /// Samples in `current`.
    in_current: u32,
}

impl<T: Worst, const LEN: u32> TwoBucketWorst<T, LEN> {
    /// Fold in one sample; a bucket holds `LEN` samples, then becomes the
    /// last full one.
    pub fn push(&mut self, sample: T) {
        self.current = self.current.worst(sample);
        self.in_current += 1;
        if self.in_current == LEN {
            self.previous = std::mem::take(&mut self.current);
            self.in_current = 0;
        }
    }

    /// The worst of the bucket being filled and of the last full one.
    pub fn worst(&self) -> T {
        self.current.worst(self.previous)
    }
}

/// At most one WARN per period of a timeline (100 ns).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WarnLimiter {
    /// The instant of the last WARN; `None` before the first.
    last_100ns: Option<i64>,
    /// WARN-worthy events held back since the last WARN.
    suppressed: u64,
}

impl WarnLimiter {
    /// A WARN-worthy event at `at_100ns`. `Some(held back since the last
    /// WARN)` when no WARN went out in the `every_100ns` before it (it is
    /// to be written, and the count restarts); `None` when it is held back
    /// (counted).
    pub fn admit(&mut self, at_100ns: i64, every_100ns: i64) -> Option<u64> {
        let quiet = self
            .last_100ns
            .is_none_or(|last| at_100ns >= last + every_100ns);
        if !quiet {
            self.suppressed += 1;
            return None;
        }
        self.last_100ns = Some(at_100ns);
        Some(std::mem::take(&mut self.suppressed))
    }
}

#[cfg(test)]
#[path = "stat_window_tests.rs"]
mod tests;
