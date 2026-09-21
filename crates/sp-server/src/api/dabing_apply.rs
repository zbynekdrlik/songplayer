//! Live-first dub-mix apply seam (#184 round A).
//!
//! A dub-mix change must be HEARD immediately. The live engine push is awaited
//! BEFORE the DB persist, so a preset/fader change reaches the mix gains in
//! ~1.6 s (the `StemMixReader` ramp + ring) instead of waiting behind a
//! contended pool `acquire()` — sqlx's 30 s default was the owner's reported
//! "~30 s to apply" delay. A persist failure is surfaced to the caller (logged
//! + 500) but the live change has ALREADY happened.
//!
//! The ORDER is the whole point, so it lives in this pure seam: a unit test
//! records the call order and asserts the push runs even when persist errors,
//! without touching the DB or the engine channel.

use std::future::Future;

/// Await `push` (the live engine command) and `persist` (the DB write) and
/// return the persist result.
///
/// RED (#184): this awaits **persist first** — the order the GREEN commit flips.
pub async fn apply_dub_mix<T, E, PushFut, PersistFut>(
    push: PushFut,
    persist: PersistFut,
) -> Result<T, E>
where
    PushFut: Future<Output = ()>,
    PersistFut: Future<Output = Result<T, E>>,
{
    let out = persist.await;
    push.await;
    out
}

#[cfg(test)]
#[path = "dabing_apply_tests.rs"]
mod tests;
