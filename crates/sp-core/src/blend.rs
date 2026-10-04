//! The fade weight both program outputs blend with (pure, WASM-safe).
//!
//! `SP-program`'s fade (#215, `sp-server` `program_transition::weight_q8`)
//! weights the incoming picture in Q8, and the `SP-program-MAX` compositor
//! (#223, `sp-gpu` `Composition::Fade`) takes that same weight, so the two
//! outputs of one boundary blend alike. One unit for both; its users' tests
//! pin what it does (sp-server's blend pins, sp-gpu's layer weights).

/// The Q8 weight of the incoming (`to`) picture that is all of it: 0 = all
/// `from`, 256 = all `to`.
pub const Q8_ONE: u32 = 256;
