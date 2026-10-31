//! Tests for a compositor's targets (#251 step 2): the primary for life,
//! added ones drawn after it in the order added, ids never reused.

use super::{TargetId, Targets};
use crate::composition::{FHD_TARGET, MAX_TARGET, Target};

/// A third target, for the order and the ids.
const SMALL: Target = Target {
    width: 640,
    height: 360,
    fit: crate::composition::Fit::Aspect,
};

fn order(targets: &Targets<Target>) -> Vec<(TargetId, Target)> {
    targets.iter().map(|(id, target)| (id, *target)).collect()
}

#[test]
fn a_compositor_starts_with_its_primary_target_alone() {
    let targets = Targets::new(MAX_TARGET);
    assert_eq!(order(&targets), [(TargetId::PRIMARY, MAX_TARGET)]);
    assert_eq!(*targets.primary(), MAX_TARGET);
    assert_eq!(targets.get(TargetId::PRIMARY), Some(&MAX_TARGET));
}

#[test]
fn added_targets_are_drawn_after_the_primary_in_the_order_added() {
    let mut targets = Targets::new(MAX_TARGET);
    let fhd = targets.add(FHD_TARGET);
    let small = targets.add(SMALL);
    assert_ne!(fhd, TargetId::PRIMARY);
    assert_ne!(small, fhd, "each target its own id");
    assert_ne!(small, TargetId::PRIMARY);
    assert_eq!(
        order(&targets),
        [
            (TargetId::PRIMARY, MAX_TARGET),
            (fhd, FHD_TARGET),
            (small, SMALL)
        ]
    );
    assert_eq!(targets.get(fhd), Some(&FHD_TARGET));
    assert_eq!(targets.get(small), Some(&SMALL));
    assert_eq!(*targets.primary(), MAX_TARGET);
}

#[test]
fn a_removed_target_is_given_back_and_no_longer_drawn() {
    let mut targets = Targets::new(MAX_TARGET);
    let fhd = targets.add(FHD_TARGET);
    let small = targets.add(SMALL);
    assert_eq!(targets.remove(fhd), Some(FHD_TARGET), "the first added");
    assert_eq!(
        order(&targets),
        [(TargetId::PRIMARY, MAX_TARGET), (small, SMALL)]
    );
    assert_eq!(targets.get(fhd), None);
    assert_eq!(targets.remove(fhd), None, "removed once");
    assert_eq!(targets.remove(small), Some(SMALL), "the last one");
    assert_eq!(order(&targets), [(TargetId::PRIMARY, MAX_TARGET)]);
}

#[test]
fn the_primary_target_is_never_removed() {
    let mut targets = Targets::new(MAX_TARGET);
    let fhd = targets.add(FHD_TARGET);
    assert_eq!(targets.remove(TargetId::PRIMARY), None);
    assert_eq!(
        order(&targets),
        [(TargetId::PRIMARY, MAX_TARGET), (fhd, FHD_TARGET)]
    );
    let mut alone = Targets::new(MAX_TARGET);
    assert_eq!(alone.remove(TargetId::PRIMARY), None, "even alone");
    assert_eq!(*alone.primary(), MAX_TARGET);
}

#[test]
fn an_id_is_never_reused_after_its_target_is_removed() {
    let mut targets = Targets::new(MAX_TARGET);
    let first = targets.add(FHD_TARGET);
    targets.remove(first);
    let second = targets.add(FHD_TARGET);
    let third = targets.add(SMALL);
    assert_ne!(second, first, "a new id for the target added again");
    assert_ne!(third, first);
    assert_ne!(third, second);
    assert_eq!(targets.get(first), None, "the old id names nothing");
    assert_eq!(targets.get(second), Some(&FHD_TARGET));
}

#[test]
fn a_target_is_changed_through_its_id_alone() {
    let mut targets = Targets::new(1_u32);
    let added = targets.add(2);
    *targets.get_mut(added).expect("the added target") += 10;
    *targets.get_mut(TargetId::PRIMARY).expect("the primary") += 100;
    assert_eq!(targets.get_mut(TargetId(99)), None, "no such target");
    for (_, value) in targets.iter_mut() {
        *value += 1_000;
    }
    let values: Vec<(TargetId, u32)> = targets.iter().map(|(id, v)| (id, *v)).collect();
    assert_eq!(values, [(TargetId::PRIMARY, 1_101), (added, 1_012)]);
}
