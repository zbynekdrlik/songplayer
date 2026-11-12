//! #305 item 3: [`MockNdiSourceFinder`] — a scripted [`NdiSourceFinder`] for
//! Linux tests (`sp_ndi::test_util`). A test sets the list the next polls
//! return and reads how many polls were made and with which wait.

use std::sync::Mutex;

use crate::source_finder::{FoundSource, NdiSourceFinder};

/// A scripted [`NdiSourceFinder`].
#[derive(Default)]
pub struct MockNdiSourceFinder {
    sources: Mutex<Vec<FoundSource>>,
    /// The `wait_ms` of every poll, in order.
    waits: Mutex<Vec<u32>>,
}

impl MockNdiSourceFinder {
    /// The list every following poll returns.
    pub fn set_sources(&self, sources: Vec<FoundSource>) {
        *self.sources.lock().unwrap() = sources;
    }

    /// The `wait_ms` of every poll made so far, in order.
    pub fn polls(&self) -> Vec<u32> {
        self.waits.lock().unwrap().clone()
    }
}

impl NdiSourceFinder for MockNdiSourceFinder {
    fn current_sources(&self, wait_ms: u32) -> Vec<FoundSource> {
        self.waits.lock().unwrap().push(wait_ms);
        self.sources.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(name: &str, url: &str) -> FoundSource {
        FoundSource {
            name: name.into(),
            url: url.into(),
        }
    }

    #[test]
    fn the_mock_returns_the_scripted_list_and_records_each_poll() {
        let mock = MockNdiSourceFinder::default();
        assert!(mock.polls().is_empty());
        assert!(mock.current_sources(250).is_empty());
        mock.set_sources(vec![
            src("PC (SP-program)", "10.0.0.1:5961"),
            src("PC (x)", ""),
        ]);
        assert_eq!(
            mock.current_sources(1000),
            vec![src("PC (SP-program)", "10.0.0.1:5961"), src("PC (x)", "")]
        );
        assert_eq!(mock.polls(), vec![250, 1000]);
    }
}
