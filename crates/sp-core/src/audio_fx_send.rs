//! #278: when the playlist EQ panel sends its settings.
//!
//! There is no "Použiť" any more: every change (a drag, a slider, a
//! switch, the volume) goes to `PUT /api/v1/playlists/{id}/audio` live, and
//! the playing song follows through the server's 50 ms crossfade. The panel
//! sends the settings AS THEY ARE at the send (the latest value wins), at
//! most one request every [`THROTTLE_MS`], never a second one while one is
//! in flight, and a drag's end sends at once ([`SendGate::flush`]) so its
//! last position is never left waiting. The rule is this pure state
//! machine; the panel only carries out its [`SendStep`]s.

/// The least time between the starts of two sends, ms.
pub const THROTTLE_MS: f64 = 50.0;

/// How long the panel waits for a send's answer, ms. A send not answered
/// by then is given up ([`SendGate::landed`], "server neodpovedá"), so a
/// server that never answers can never hold the gate in flight for good. A
/// request given up that still lands later is watched by the panel
/// ([`late_answer`]): if the settings changed meanwhile, they go out once
/// more (it may have landed over a newer save).
pub const ANSWER_TIMEOUT_MS: u32 = 10_000;

/// The status line while a change waits or a request runs.
pub const SAVING_SK: &str = "Ukladám…";
/// The status line once the last change is saved.
pub const SAVED_SK: &str = "Uložené";
/// The status line when a send was given up after [`ANSWER_TIMEOUT_MS`].
pub const NO_ANSWER_SK: &str = "Uloženie zlyhalo: server neodpovedá";

/// The status line for a send's answer: its error, else "Ukladám…" while
/// the gate is still `busy` (a newer change waits or goes out), else
/// "Uložené".
pub fn answer_sk(answer: &Result<(), String>, busy: bool) -> String {
    match answer {
        Err(e) => format!("Uloženie zlyhalo: {e}"),
        Ok(()) if busy => SAVING_SK.to_string(),
        Ok(()) => SAVED_SK.to_string(),
    }
}

/// What the late answer of a request given up after [`ANSWER_TIMEOUT_MS`]
/// asks of the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LateAnswer {
    /// Send the settings as they are once more.
    Resend,
    /// Say "Uložené": it was the save after all.
    Saved,
    /// Nothing.
    Nothing,
}

/// A given-up request landed: `landed_ok` its answer, `changed_since`
/// whether the settings differ from what it carried, `busy` the gate's.
/// Newer settings may have been written over by it — an error too: the
/// server may have written the row before it failed — so they go out once
/// more; one landing ok over unchanged settings, with nothing going out,
/// was the save.
pub fn late_answer(landed_ok: bool, changed_since: bool, busy: bool) -> LateAnswer {
    if changed_since {
        LateAnswer::Resend
    } else if landed_ok && !busy {
        LateAnswer::Saved
    } else {
        LateAnswer::Nothing
    }
}

/// What the panel does now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SendStep {
    /// Nothing.
    Idle,
    /// Send the settings as they are now; call [`SendGate::landed`] when
    /// the answer is in (or the send was given up).
    Send,
    /// Arm ONE timer for this many ms, then call [`SendGate::tick`].
    WaitMs(f64),
}

/// The panel's send state. All times are one monotonic clock, ms.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SendGate {
    /// When the last send started.
    last_start_ms: Option<f64>,
    /// A send waits for its answer.
    in_flight: bool,
    /// The settings changed since the last send started.
    dirty: bool,
    /// A drag ended: send the change without waiting for the throttle.
    flush: bool,
    /// A timer is armed for the throttle's end.
    armed: bool,
}

impl SendGate {
    /// The operator changed something.
    pub fn changed(&mut self, now_ms: f64) -> SendStep {
        self.dirty = true;
        self.next(now_ms)
    }

    /// A drag (or another gesture) ended: its last value goes out at once,
    /// or the moment the request in flight is answered.
    pub fn flush(&mut self, now_ms: f64) -> SendStep {
        self.flush = self.dirty;
        self.next(now_ms)
    }

    /// The request in flight was answered (or given up).
    pub fn landed(&mut self, now_ms: f64) -> SendStep {
        self.in_flight = false;
        self.next(now_ms)
    }

    /// The armed timer fired.
    pub fn tick(&mut self, now_ms: f64) -> SendStep {
        self.armed = false;
        self.next(now_ms)
    }

    /// A change waits or a request runs: the status line says "Ukladám…".
    pub fn busy(&self) -> bool {
        self.dirty || self.in_flight
    }

    fn next(&mut self, now_ms: f64) -> SendStep {
        if !self.dirty || self.in_flight {
            return SendStep::Idle;
        }
        let due = self.last_start_ms.map_or(now_ms, |at| at + THROTTLE_MS);
        if self.flush || now_ms >= due {
            self.dirty = false;
            self.flush = false;
            self.in_flight = true;
            self.last_start_ms = Some(now_ms);
            return SendStep::Send;
        }
        if self.armed {
            return SendStep::Idle;
        }
        self.armed = true;
        SendStep::WaitMs(due - now_ms)
    }
}

#[cfg(test)]
#[path = "audio_fx_send_tests.rs"]
mod tests;
