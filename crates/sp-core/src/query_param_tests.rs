//! The one whole-count query parse (`hours`, `days`).

use super::whole_count;

#[test]
fn an_absent_count_is_the_default() {
    assert_eq!(whole_count(None, 24, 2160), Some(24));
    assert_eq!(whole_count(None, 400, 400), Some(400));
}

#[test]
fn a_count_from_1_to_max_is_taken_leading_zeros_too() {
    assert_eq!(whole_count(Some("1"), 24, 2160), Some(1));
    assert_eq!(whole_count(Some("2160"), 24, 2160), Some(2160));
    assert_eq!(whole_count(Some("0168"), 24, 2160), Some(168));
    assert_eq!(whole_count(Some("37"), 400, 400), Some(37));
}

#[test]
fn anything_else_is_refused() {
    for bad in [
        "0",
        "2161",
        "",
        "abc",
        "-1",
        "+5",
        " 7",
        "7 ",
        "1.5",
        "99999999999",
    ] {
        assert_eq!(whole_count(Some(bad), 24, 2160), None, "{bad:?}");
    }
}
