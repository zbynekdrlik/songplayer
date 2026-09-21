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

/// Await `push` (the live engine command) FIRST, then `persist` (the DB write),
/// returning the persist result. The order — push before persist — is the whole
/// point of the seam: the live change is heard immediately and never blocks on a
/// contended pool acquire.
pub async fn apply_dub_mix<T, E, PushFut, PersistFut>(
    push: PushFut,
    persist: PersistFut,
) -> Result<T, E>
where
    PushFut: Future<Output = ()>,
    PersistFut: Future<Output = Result<T, E>>,
{
    push.await;
    persist.await
}

#[cfg(test)]
#[path = "dabing_apply_tests.rs"]
mod tests;
