//! #233: a scripted ASIO driver for the worker's tests (CI runners have no
//! ASIO driver): scripted open answers and messages; `drain` plays the card,
//! taking `buffer` frames per callback from the ring like the real callback
//! (`asio_win.rs`: a short ring is silence, an underrun once primed).

use std::collections::VecDeque;

use super::*;

pub(crate) struct FakeDevice {
    pub opens: VecDeque<Result<Opened, Reason>>,
    pub events: VecDeque<DeviceEvents>,
    pub ring: Option<rtrb::Consumer<f32>>,
    pub buffer: u32,
    pub consumed: u64,
    pub underruns: u64,
    pub callbacks: u64,
    pub primed: bool,
    pub opened: Vec<(String, [u32; 2])>,
    pub starts: u32,
    pub closes: u32,
    pub played: Vec<f32>,
}

/// Dante Virtual Soundcard as the box runs it: 128-frame buffers, Int32.
pub(crate) fn dvs(rate: f64) -> Opened {
    Opened {
        rate,
        buffer_frames: 128,
        out_channels: 2,
        sample: AsioSample::Int32,
    }
}

impl FakeDevice {
    /// A driver whose opens answer `opens` in turn, then open at 96 kHz.
    pub fn answering(opens: Vec<Result<Opened, Reason>>) -> Self {
        Self {
            opens: opens.into(),
            events: VecDeque::new(),
            ring: None,
            buffer: 128,
            consumed: 0,
            underruns: 0,
            callbacks: 0,
            primed: false,
            opened: Vec::new(),
            starts: 0,
            closes: 0,
            played: Vec::new(),
        }
    }

    /// The card takes `n` callbacks.
    pub fn drain(&mut self, n: u32) {
        for _ in 0..n {
            let want = self.buffer as usize * 2;
            if let Some(ring) = self.ring.as_mut() {
                let mut got = vec![0.0f32; want];
                let n_got = ring.pop_partial_slice(&mut got).0.len();
                self.played.extend_from_slice(&got[..n_got]);
                if n_got < want && self.primed {
                    self.underruns += 1;
                }
            }
            self.consumed += u64::from(self.buffer);
            self.callbacks += 1;
        }
    }

    /// Frames in the ring now.
    pub fn ring_frames(&self) -> usize {
        self.ring.as_ref().map_or(0, |r| r.slots() / 2)
    }
}

impl AsioDevice for FakeDevice {
    fn open(&mut self, driver: &str, channels: [u32; 2]) -> Result<Opened, Reason> {
        self.opened.push((driver.to_string(), channels));
        let answer = self.opens.pop_front().unwrap_or_else(|| Ok(dvs(96_000.0)));
        if let Ok(o) = &answer {
            self.buffer = o.buffer_frames;
        }
        answer
    }

    fn start(&mut self, ring: rtrb::Consumer<f32>) -> Result<Started, Reason> {
        self.ring = Some(ring);
        self.starts += 1;
        self.consumed = 0;
        self.callbacks = 0;
        self.underruns = 0;
        self.primed = false;
        Ok(Started {
            output_latency_frames: 128,
        })
    }

    fn poll(&mut self) -> DeviceEvents {
        let mut ev = self.events.pop_front().unwrap_or_default();
        ev.callbacks = self.callbacks;
        ev
    }

    fn consumed_frames(&self) -> u64 {
        self.consumed
    }

    fn underruns(&self) -> u64 {
        self.underruns
    }

    fn mark_primed(&mut self) {
        self.primed = true;
    }

    fn close(&mut self) {
        self.ring = None;
        self.closes += 1;
    }
}
