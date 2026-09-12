//! Monotonic-to-UTC wall clock for genlocked NDI timecodes (#146).
//! Implementation follows in the GREEN commit; this shell only wires the
//! RED tests.

#[cfg(test)]
#[path = "wallclock_tests.rs"]
mod wallclock_tests;
