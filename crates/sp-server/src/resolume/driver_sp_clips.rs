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
//!   refreshed then; the episode's own refreshes evaluate their bodies). The
//!   whole poll is bounded by `sp_clips::POLL_BUDGET` (2 s): it runs inside
//!   the driver's step, ahead of the title and lyric pushes (review round 1).
//! - **A param answered 404** is a stale map (Arena re-ids every clip and
//!   param on relaunch): the push path's own stale-map refresh
//!   (`refresh_stale_map`: the refused-id hold, the not-ready episode, the
//!   refresh through `decide`). It fires no RecoveryEvent of its own (the
//!   refresh's ready transition does, as for a push) and never counts
//!   toward the breaker, like the param probe (`driver_probe.rs`). An
//!   answered refresh evaluates its body; else the fact is unknown. A
//!   refresh that maps the same clips while the 404'd param is still
//!   tracked (a refused id) holds the poll until the next full refresh: no
//!   14 MB fetch every minute (review round 1).
//! - **Anything else that does not read** (a 5xx, a timeout, the budget)
//!   makes the fact unknown until the next read; an answer that does not
//!   decode as a param is a reader fault (`Fact::reader_fault`: SongPlayer's
//!   own reader is wrong, the post-deploy gate fails on it).
//! - **The outage** (`apply_outcome` opening the breaker) forgets the
//!   tracked clips: unknown until a refresh tracks them again.
//!
//! Read only: no request here writes anything (no PUT, no POST, no clip
//! route), pinned by `sp_clips_tests.rs`'s source scan.

use std::time::Instant;

use tracing::{debug, info};

use super::push::StaleRefresh;
use super::{HostDriver, ResolvedEndpoint};
use crate::resolume::sp_clip_store::SpClipStore;
use crate::resolume::sp_clips::{self, Fact, ParamAnswer, ParamId, ParamValue, Read, Values};

/// What one poll read.
enum Polled {
    /// Every tracked param answered.
    Values(Values),
    /// A param answered 404: the map is stale.
    Stale(ParamId),
    /// A param did not read: the unknown fact, and why.
    Unread(Fact),
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
        self.sp_clips.held = false;
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
        let budget = self.sp_clips.budget();
        let params = self.sp_clips.tracked.params().len();
        let started = Instant::now();
        let polled = tokio::time::timeout(budget, self.read_sp_params())
            .await
            .unwrap_or_else(|_| {
                Polled::Unread(Fact::unknown(&format!(
                    "the poll of {params} params took over {budget:?}"
                )))
            });
        debug!(
            host = %self.host,
            params,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "SongPlayer clip params polled"
        );
        match polled {
            Polled::Values(values) => {
                let fact = sp_clips::evaluate(&self.sp_clips.tracked, &values);
                self.set_sp_fact(fact);
            }
            Polled::Unread(fact) => self.set_sp_fact(fact),
            Polled::Stale(param_id) => self.stale_sp_param(param_id, now).await,
        }
    }

    /// A tracked param answered 404: refresh the stale map the push path's
    /// way. A refresh that read a body gave the fact; with none, the tracked
    /// ids are gone and nothing is known. The same clips with the 404'd param
    /// still tracked is a refused id: the poll holds until the next refresh.
    async fn stale_sp_param(&mut self, param_id: ParamId, now: Instant) {
        info!(
            host = %self.host,
            param_id,
            "Resolume answered 404 for a tracked SongPlayer clip param — the clip map is stale"
        );
        let refreshed = self.refresh_stale_map(now).await;
        if self.sp_clips.body_at != Some(now) {
            self.set_sp_fact(Fact::unknown(&format!(
                "param {param_id} answered 404 and no refresh read the composition"
            )));
        } else if refreshed == StaleRefresh::SameMap
            && self.sp_clips.tracked.params().contains(&param_id)
        {
            self.sp_clips.held = true;
            info!(
                host = %self.host,
                param_id,
                "Resolume still lists the SongPlayer clip param it answered 404 for — the clip poll waits for the next full refresh"
            );
        }
    }

    /// Every tracked param, one after another; the first that does not read
    /// ends the poll.
    async fn read_sp_params(&mut self) -> Polled {
        let ep = match self.ensure_endpoint().await {
            Ok(ep) => ep,
            Err(e) => {
                return Polled::Unread(Fact::unknown(&format!("no Resolume endpoint: {e}")));
            }
        };
        let mut values = Values::new();
        for param_id in self.sp_clips.tracked.params() {
            match self.read_param(&ep, param_id).await {
                Ok(Some(value)) => {
                    values.insert(param_id, value);
                }
                Ok(None) => return Polled::Stale(param_id),
                Err(e) => return Polled::Unread(unread_param(param_id, &e)),
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

/// The fact of a param read that failed: an answer that does not decode as
/// a param (serde's own error under reqwest's) is SongPlayer's own reader
/// failing (a reader fault); a status, a transport error or a body cut off
/// mid-read (reqwest calls that a decode error too, review round 2) is
/// Arena not answering.
fn unread_param(param_id: ParamId, e: &anyhow::Error) -> Fact {
    let why = format!("param {param_id}: {e}");
    if e.downcast_ref::<reqwest::Error>()
        .and_then(std::error::Error::source)
        .is_some_and(|source| source.is::<serde_json::Error>())
    {
        Fact::reader_fault(&why)
    } else {
        Fact::unknown(&why)
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
