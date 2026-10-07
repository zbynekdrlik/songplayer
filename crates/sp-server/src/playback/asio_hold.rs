//! #233 (review round 1): the ASIO drivers this process holds. An edited
//! ASIO entry is rebuilt, and its successor's worker starts while the old
//! worker still stops, disposes and releases the same driver; a second
//! instance of one driver inside one process is something many ASIO drivers
//! do not expect (one client each). So a device takes the driver's name here
//! BEFORE it loads the driver, and gives it back only after the driver is
//! released (`asio_win.rs`); a successor that finds it held is refused as
//! busy and tries again after the backoff (2 s, `asio_state::BACKOFF_S`).

use std::sync::Mutex;

use crate::playback::audio_out_queue::lock;

/// The drivers held in this process (production: `asio_win`'s static).
#[derive(Default)]
pub struct DriverHolds {
    held: Mutex<Vec<String>>,
}

/// One held driver, given back when dropped.
pub struct DriverHold<'a> {
    holds: &'a DriverHolds,
    driver: String,
}

impl DriverHolds {
    /// None held (a `const fn`, for a static).
    pub const fn new() -> Self {
        Self {
            held: Mutex::new(Vec::new()),
        }
    }

    /// Hold `driver`, or `None` while another holder has it.
    pub fn claim(&self, driver: &str) -> Option<DriverHold<'_>> {
        let mut held = lock(&self.held);
        if held.iter().any(|h| h == driver) {
            return None;
        }
        held.push(driver.to_string());
        Some(DriverHold {
            holds: self,
            driver: driver.to_string(),
        })
    }

    /// Whether `driver` is held now.
    pub fn is_held(&self, driver: &str) -> bool {
        lock(&self.held).iter().any(|h| h == driver)
    }
}

impl Drop for DriverHold<'_> {
    fn drop(&mut self) {
        let mut held = lock(&self.holds.held);
        if let Some(i) = held.iter().position(|h| *h == self.driver) {
            held.swap_remove(i);
        }
    }
}

#[cfg(test)]
#[path = "asio_hold_tests.rs"]
mod tests;
