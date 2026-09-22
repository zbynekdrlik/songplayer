//! Live-first mix apply seam (#184 round A, generalised in round G).
//!
//! A mixer fader change must be HEARD immediately. The live engine push is awaited
//! BEFORE the settings persist, so a fader/preset change reaches the mix gains in
//! ~1.6 s (the `StemMixReader` ramp + ring) instead of waiting behind a contended
//! pool `acquire()` — sqlx's 30 s default was the owner's reported "~30 s to
//! apply" delay. A persist failure is surfaced to the caller (logged + 500) but
//! the live change has ALREADY happened.
//!
//! The ORDER is the whole point, so it lives in this pure seam: a unit test
//! records the call order and asserts the push runs even when persist errors,
//! without touching the DB or the engine channel.

use std::future::Future;

/// Await `push` (the live engine command) FIRST, then `persist` (the settings
/// write), returning the persist result. The order — push before persist — is the
/// whole point: the live change is heard immediately and never blocks on a
/// contended pool acquire.
pub async fn apply_mix<T, E, PushFut, PersistFut>(
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
#[path = "mix_apply_tests.rs"]
mod tests;
