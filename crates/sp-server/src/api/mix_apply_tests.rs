//! Order tests for the live-first mix apply seam (#184 round A/G).

use super::apply_mix;
use std::cell::RefCell;
use std::rc::Rc;

/// The push (live engine command) MUST be awaited BEFORE the persist (settings
/// write) — the whole reason the seam exists (#184: the mix must change in ~1.6 s,
/// not wait behind a contended pool acquire).
#[tokio::test]
async fn push_is_awaited_before_persist() {
    let order: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));
    let op = order.clone();
    let os = order.clone();
    let res: Result<(), ()> = apply_mix(
        async move {
            op.borrow_mut().push("push");
        },
        async move {
            os.borrow_mut().push("persist");
            Ok(())
        },
    )
    .await;
    assert!(res.is_ok());
    assert_eq!(*order.borrow(), vec!["push", "persist"]);
}

/// A persist failure is returned to the caller, but the live push has already
/// happened — order-independent, so this holds regardless of the seam order.
#[tokio::test]
async fn push_happens_even_when_persist_errors() {
    let order: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));
    let op = order.clone();
    let os = order.clone();
    let res: Result<(), &'static str> = apply_mix(
        async move {
            op.borrow_mut().push("push");
        },
        async move {
            os.borrow_mut().push("persist");
            Err("boom")
        },
    )
    .await;
    assert_eq!(res, Err("boom"));
    assert!(
        order.borrow().contains(&"push"),
        "the live push must run even when the persist fails"
    );
}
