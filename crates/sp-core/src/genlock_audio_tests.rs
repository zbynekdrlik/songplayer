//! Paced-audio grid math tests (#148): `samples_per_boundary`. `super::*`
//! resolves to the `genlock::audio` module under test.

use super::*;

#[test]
fn samples_per_boundary_exact_grid_rates() {
    assert_eq!(samples_per_boundary(48_000, 30), 1600);
    assert_eq!(samples_per_boundary(48_000, 60), 800);
}

#[test]
fn samples_per_boundary_zero_fps_is_zero_no_panic() {
    assert_eq!(samples_per_boundary(48_000, 0), 0);
    assert_eq!(samples_per_boundary(0, 30), 0);
}
