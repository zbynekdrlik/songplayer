//! #275: the Resolume driver's read of whether SongPlayer's clip runs in
//! Arena. Split from `driver.rs` for the 1000-line cap; a child module of
//! `driver`, so it reads the driver's private state. The rule is the pure
//! `resolume::sp_clips` (`track`, `evaluate`); the fact goes to the driver's
//! own `sp_clips.fact` and, in production, into `sp_clip_store::global()`
//! (`GET /api/v1/wall`).
//!
//! - **A full refresh** (`refresh_mapping`) tracks the clips of its body and
//!   evaluates the values that body already carries: no extra request.
//! - **Between refreshes** a liveness tick reads the tracked params by id
//!   (`GET /api/v1/parameter/by-id/{id}`, ~140 B each, one after another,
//!   stopping at the first that does not answer) once its last read is
//!   `sp_clips::POLL_EVERY` old — only with the breaker closed, an answered
//!   probe on this tick and no not-ready episode open (a relaunch is being
//!   refreshed then; the episode's own refreshes evaluate their bodies).
//! - **A param answered 404** is a stale map (Arena re-ids every clip and
//!   param on relaunch): the push path's own stale-map refresh
//!   (`refresh_stale_map`: the refused-id hold, the not-ready episode, the
//!   refresh through `decide`). It fires no RecoveryEvent of its own (the
//!   refresh's ready transition does, as for a push) and never counts
//!   toward the breaker, like the param probe (`driver_probe.rs`). An
//!   answered refresh evaluates its body; else the fact is unknown.
//! - **Anything else that does not read** (a 5xx, a timeout, a body that is
//!   no param) makes the fact unknown until the next read.
//! - **The outage** (`apply_outcome` opening the breaker) forgets the
//!   tracked clips: unknown until a refresh tracks them again.
//!
//! Read only: no request here writes anything (no PUT, no POST, no clip
//! route), pinned by `sp_clips_tests.rs`'s source scan.

use std::time::Instant;

use tracing::{debug, info};

use super::{HostDriver, ResolvedEndpoint};
use crate::resolume::sp_clip_store::SpClipStore;
use crate::resolume::sp_clips::{self, Fact, ParamAnswer, ParamId, ParamValue, Read, Values};

/// What one poll read.
enum Polled {
    /// Every tracked param answered.
    Values(Values),
    /// A param answered 404: the map is stale.
    Stale(ParamId),
    /// A param did not read (why).
    Unread(String),
}

impl HostDriver {
    /// Publish this host's fact into `store` (production:
    /// `sp_clip_store::global()`, set by `ResolumeRegistry::add_host`). A
    /// driver with no store keeps its fact to itself (`sp_clips.fact`).
    pub fn with_sp_clip_store(mut self, store: &'static SpClipStore) -> Self {
        self.sp_clips.store = Some(store);
        self
    }

    /// A full refresh's body (`refresh_mapping`): track its clips and
    /// evaluate the values it carries. It is this tick's read: no poll
    /// follows it for `POLL_EVERY`.
    pub(super) fn note_sp_clips(&mut self, read: Read, now: Instant) {
        let fact = sp_clips::evaluate(&read.tracked, &read.values);
        self.sp_clips.tracked = read.tracked;
        self.sp_clips.read_at = Some(now);
        self.sp_clips.body_at = Some(now);
        self.set_sp_fact(fact);
    }

    /// The outage evicted the clip map (`apply_outcome` opened the breaker):
    /// Arena does not answer, so nothing is known until a refresh tracks the
    /// clips again.
    pub(super) fn forget_sp_clips(&mut self) {
        self.sp_clips.tracked = sp_clips::Tracked::NotLoaded;
        self.sp_clips.read_at = None;
        self.set_sp_fact(Fact::unknown(
            "Arena does not answer (the circuit breaker is open)",
        ));
    }

    /// One poll step (`on_tick_at`, after the tick's refresh): read the
    /// tracked params by id when a read is due (module doc).
    pub(super) async fn poll_sp_clips(&mut self, now: Instant) {
        if self.circuit_breaker_open
            || self.not_ready_since.is_some()
            || !self.sp_clips.poll_due(now)
        {
            return;
        }
        self.sp_clips.read_at = Some(now);
        match self.read_sp_params().await {
            Polled::Values(values) => {
                let fact = sp_clips::evaluate(&self.sp_clips.tracked, &values);
                self.set_sp_fact(fact);
            }
            Polled::Unread(why) => self.set_sp_fact(Fact::unknown(&why)),
            Polled::Stale(param_id) => {
                info!(
                    host = %self.host,
                    param_id,
                    "Resolume answered 404 for a tracked SongPlayer clip param — the clip map is stale"
                );
                self.refresh_stale_map(now).await;
                if self.sp_clips.body_at != Some(now) {
                    // No answered refresh evaluated a body: the tracked ids
                    // are gone, nothing is known.
                    self.set_sp_fact(Fact::unknown(&format!(
                        "param {param_id} answered 404 and no refresh read the composition"
                    )));
                }
            }
        }
    }

    /// Every tracked param, one after another; the first that does not read
    /// ends the poll.
    async fn read_sp_params(&mut self) -> Polled {
        let ep = match self.ensure_endpoint().await {
            Ok(ep) => ep,
            Err(e) => return Polled::Unread(format!("no Resolume endpoint: {e}")),
        };
        let mut values = Values::new();
        for param_id in self.sp_clips.tracked.params() {
            match self.read_param(&ep, param_id).await {
                Ok(Some(value)) => {
                    values.insert(param_id, value);
                }
                Ok(None) => return Polled::Stale(param_id),
                Err(e) => return Polled::Unread(format!("param {param_id}: {e}")),
            }
        }
        Polled::Values(values)
    }

    /// `GET /api/v1/parameter/by-id/{param_id}`: its value, `None` on a 404.
    async fn read_param(
        &self,
        ep: &ResolvedEndpoint,
        param_id: ParamId,
    ) -> Result<Option<ParamValue>, anyhow::Error> {
        let url = format!("{}/api/v1/parameter/by-id/{param_id}", ep.base_url);
        let resp = Self::apply_host_header(self.client.get(&url), ep)
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let answer: ParamAnswer = resp.error_for_status()?.json().await?;
        Ok(Some(answer.value))
    }

    /// The new fact: logged when it changes, published into the store (if
    /// any) and kept.
    fn set_sp_fact(&mut self, fact: Fact) {
        log_sp_fact(&self.host, &self.sp_clips.fact, &fact);
        if let Some(store) = self.sp_clips.store {
            let key = format!("{}:{}", self.host, self.port);
            store.publish(&key, fact.clone(), chrono::Utc::now().timestamp_millis());
        }
        self.sp_clips.fact = fact;
    }
}

/// The log line of a changed fact: one INFO per change of state, a DEBUG
/// line for another clip set or reason in the same state.
#[cfg_attr(test, mutants::skip)] // log text only; the facts are pinned by the driver's tests
fn log_sp_fact(host: &str, old: &Fact, new: &Fact) {
    let names = |fact: &Fact| -> String {
        let each: Vec<String> = fact.clips.iter().map(|c| c.text()).collect();
        each.join("; ")
    };
    let why = new.why.as_deref().unwrap_or("");
    if old.state != new.state {
        info!(
            host,
            from = old.state.name(),
            to = new.state.name(),
            clips = %names(new),
            why,
            "SongPlayer's clip in Resolume: {} -> {}",
            old.state.name(),
            new.state.name()
        );
    } else if old != new {
        debug!(host, state = new.state.name(), clips = %names(new), why, "SongPlayer's clip in Resolume changed");
    }
}

#[cfg(test)]
#[path = "driver_sp_clips_tests.rs"]
mod tests;
