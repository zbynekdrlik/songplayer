//! #305 item 3: the discoverability decisions (`ndi_discover.rs`) — the
//! expected name, the Dante check, which listing counts, the state machine
//! with its grace and its loss after three reads, and one read through the
//! mock finder.

use sp_ndi::FoundSource;
use sp_ndi::test_util::MockNdiSourceFinder;

use super::{
    At, DISCOVER_GRACE_MS, DISCOVER_LOST_READS, DISCOVER_STALE_MS, DISCOVER_WAIT_MS, DiscoverLog,
    DiscoverState, DiscoverStatus, Discovery, NOT_DISCOVERABLE_REASON, NdiDiscover, Sighting,
    expected_name, on_dante, sighting,
};
use crate::dante_guard::Cidr;

const NAME: &str = "RESOLUME-SNV (SP-program)";

fn src(name: &str, url: &str) -> FoundSource {
    FoundSource {
        name: name.into(),
        url: url.into(),
    }
}

fn dante() -> Vec<Cidr> {
    vec![Cidr::parse("10.77.7.0/24").unwrap()]
}

fn seen(address: &str) -> Option<Sighting> {
    Some(Sighting {
        address: address.into(),
        on_dante: false,
    })
}

/// UTC sits this far from the monotonic clock, so a test sees which clock a
/// value came from.
const UTC: i64 = 1_700_000_000_000;

fn at(t: i64) -> At {
    At {
        mono_ms: t,
        utc_ms: UTC + t,
    }
}

fn begun(t: i64) -> Discovery {
    let mut d = Discovery::default();
    d.begin(NAME.into(), at(t));
    d
}

#[test]
fn the_expected_name_is_the_machine_then_sp_program_in_parentheses() {
    assert_eq!(
        expected_name(Some("RESOLUME-PP")).as_deref(),
        Some("RESOLUME-PP (SP-program)")
    );
    assert_eq!(
        expected_name(Some("  RESOLUME-SNV \n")).as_deref(),
        Some(NAME)
    );
    assert_eq!(expected_name(Some("   ")), None);
    assert_eq!(expected_name(None), None);
}

#[test]
fn a_dante_address_is_one_inside_a_dante_subnet() {
    assert!(on_dante("10.77.7.107:5961", &dante()));
    assert!(on_dante("10.77.7.107", &dante()), "no port");
    assert!(!on_dante("10.77.9.201:5961", &dante()));
    assert!(!on_dante("10.77.7.107:5961", &[]), "no Dante subnet known");
    assert!(!on_dante("", &dante()));
    assert!(!on_dante("host.local:5961", &dante()));
    // An IPv6 URL is unbracketed (an IPv4-mapped one is its IPv4 address).
    assert!(on_dante("[::ffff:10.77.7.107]:5961", &dante()));
    assert!(!on_dante("[fe80::1]:5961", &dante()));
}

#[test]
fn only_the_expected_name_counts_whatever_its_case() {
    let sources = [
        src("RESOLUME-SNV (SP-programX)", "10.77.9.201:5962"),
        src("OTHER (SP-program)", "10.77.9.5:5961"),
        src(" resolume-snv (sp-program) ", "10.77.9.201:5961"),
    ];
    assert_eq!(sighting(&sources, NAME, &dante()), seen("10.77.9.201:5961"));
    assert_eq!(sighting(&sources[..2], NAME, &dante()), None);
    assert_eq!(sighting(&[], NAME, &dante()), None);
}

#[test]
fn a_non_dante_listing_is_preferred_and_a_dante_one_still_counts() {
    let dante_entry = src(NAME, "10.77.7.107:5961");
    let lan = src(NAME, "10.77.9.201:5961");
    let lan2 = src(NAME, "10.77.9.202:5961");
    assert_eq!(
        sighting(
            &[dante_entry.clone(), lan.clone(), lan2.clone()],
            NAME,
            &dante()
        ),
        seen("10.77.9.201:5961"),
        "the first non-Dante entry after a Dante one"
    );
    assert_eq!(
        sighting(&[lan.clone(), dante_entry.clone()], NAME, &dante()),
        seen("10.77.9.201:5961")
    );
    assert_eq!(
        sighting(&[lan2, lan], NAME, &dante()),
        seen("10.77.9.202:5961"),
        "the first of two non-Dante entries"
    );
    let second_dante = src(NAME, "10.77.7.108:5961");
    assert_eq!(
        sighting(&[dante_entry, second_dante], NAME, &dante()),
        Some(Sighting {
            address: "10.77.7.107:5961".into(),
            on_dante: true
        }),
        "only Dante listings: the first, reported"
    );
}

#[test]
fn nothing_is_judged_before_the_check_began() {
    let mut d = Discovery::default();
    assert_eq!(d.observe(None, at(100_000)), DiscoverLog::Nothing);
    assert_eq!(d.observe(seen("a:1"), at(100_000)), DiscoverLog::Nothing);
    assert_eq!(
        d.status_at(10_000_000),
        DiscoverStatus::default(),
        "never stale"
    );
}

#[test]
fn begin_starts_the_check_and_forgets_an_earlier_state() {
    let mut d = Discovery::default();
    d.unavailable("x", at(5));
    d.begin(NAME.into(), at(1_000));
    let s = d.status_at(1_000);
    assert_eq!(s.state, DiscoverState::Starting);
    assert_eq!(s.since_ms, Some(UTC + 1_000));
    assert_eq!(s.expected.as_deref(), Some(NAME));
    assert_eq!(s.reason, None);
    assert_eq!(d.expected().as_deref(), Some(NAME));
}

#[test]
fn a_source_not_seen_inside_the_grace_is_not_judged() {
    assert_eq!(DISCOVER_GRACE_MS, 30_000);
    assert_eq!(DISCOVER_LOST_READS, 3);
    let mut d = begun(1_000);
    for t in [2_000, 3_000, 4_000, 30_999] {
        assert_eq!(d.observe(None, at(t)), DiscoverLog::Nothing, "t={t}");
    }
    let s = d.status_at(30_999);
    assert_eq!(s.state, DiscoverState::Starting);
    assert_eq!((s.reads, s.misses), (4, 4));
    assert_eq!(s.read_at_ms, Some(UTC + 30_999));
    // The grace ends 30 s after the start, inclusive.
    assert_eq!(d.observe(None, at(31_000)), DiscoverLog::Lost);
    let s = d.status_at(31_000);
    assert_eq!(s.state, DiscoverState::NotDiscoverable);
    assert_eq!(s.since_ms, Some(UTC + 31_000));
    assert_eq!(s.degraded_reason(), Some(NOT_DISCOVERABLE_REASON));
    assert_eq!(s.seen_at_ms, None);
    // One WARN per loss.
    assert_eq!(d.observe(None, at(41_000)), DiscoverLog::Nothing);
    let s = d.status_at(41_000);
    assert_eq!((s.since_ms, s.misses), (Some(UTC + 31_000), 6));
}

#[test]
fn the_grace_runs_on_the_monotonic_clock_not_the_date() {
    let mut d = begun(1_000);
    // A date step of +1 h moves UTC only: the grace is still on.
    let stepped = |t: i64| At {
        mono_ms: t,
        utc_ms: UTC + t + 3_600_000,
    };
    for t in [2_000, 3_000, 4_000] {
        assert_eq!(d.observe(None, stepped(t)), DiscoverLog::Nothing);
    }
    assert_eq!(d.status_at(4_000).state, DiscoverState::Starting);
    assert_eq!(d.status_at(4_000).read_at_ms, Some(UTC + 4_000 + 3_600_000));
}

#[test]
fn a_loss_takes_three_reads_in_a_row_past_the_grace() {
    let mut d = begun(0);
    assert_eq!(
        d.observe(seen("10.0.0.1:5961"), at(10_000)),
        DiscoverLog::FirstSeen
    );
    let s = d.status_at(10_000);
    assert_eq!(s.state, DiscoverState::Discoverable);
    assert_eq!(
        (s.since_ms, s.seen_at_ms),
        (Some(UTC + 10_000), Some(UTC + 10_000))
    );
    assert_eq!(s.degraded_reason(), None);
    assert_eq!(d.observe(None, at(40_000)), DiscoverLog::Nothing);
    assert_eq!(d.observe(None, at(50_000)), DiscoverLog::Nothing);
    assert_eq!(
        d.status_at(50_000).state,
        DiscoverState::Discoverable,
        "two misses"
    );
    // A sighting resets the run.
    assert_eq!(
        d.observe(seen("10.0.0.1:5961"), at(60_000)),
        DiscoverLog::Nothing
    );
    let s = d.status_at(60_000);
    assert_eq!(
        (s.misses, s.since_ms, s.seen_at_ms),
        (0, Some(UTC + 10_000), Some(UTC + 60_000))
    );
    for t in [70_000, 80_000] {
        assert_eq!(d.observe(None, at(t)), DiscoverLog::Nothing);
    }
    assert_eq!(d.observe(None, at(90_000)), DiscoverLog::Lost);
    let s = d.status_at(90_000);
    assert_eq!(s.state, DiscoverState::NotDiscoverable);
    assert_eq!(
        (s.since_ms, s.seen_at_ms),
        (Some(UTC + 90_000), Some(UTC + 60_000))
    );
    assert_eq!(
        s.address.as_deref(),
        Some("10.0.0.1:5961"),
        "the last address kept"
    );
}

#[test]
fn a_return_is_logged_once_with_its_new_address() {
    let mut d = begun(0);
    for t in [10_000, 20_000, 30_000] {
        d.observe(None, at(t));
    }
    assert_eq!(d.status_at(30_000).state, DiscoverState::NotDiscoverable);
    let back = Some(Sighting {
        address: "10.77.7.107:5961".into(),
        on_dante: true,
    });
    assert_eq!(d.observe(back, at(40_000)), DiscoverLog::Back);
    let s = d.status_at(40_000);
    assert_eq!(s.state, DiscoverState::Discoverable);
    assert_eq!(
        (s.since_ms, s.seen_at_ms, s.misses),
        (Some(UTC + 40_000), Some(UTC + 40_000), 0)
    );
    assert_eq!(s.address.as_deref(), Some("10.77.7.107:5961"));
    assert!(s.on_dante);
    assert_eq!(s.reads, 4);
    assert_eq!(
        d.observe(seen("10.77.9.201:5961"), at(50_000)),
        DiscoverLog::Nothing
    );
    assert!(
        !d.status_at(50_000).on_dante,
        "the latest listing is reported"
    );
}

#[test]
fn a_check_with_no_read_for_35_s_reads_stale() {
    assert_eq!(DISCOVER_STALE_MS, 35_000);
    let mut d = begun(1_000);
    // Before the first read the start counts.
    assert_eq!(d.status_at(36_000).state, DiscoverState::Starting);
    assert_eq!(d.status_at(36_001).state, DiscoverState::Stale);
    d.observe(seen("10.0.0.1:5961"), at(40_000));
    assert_eq!(d.status_at(75_000).state, DiscoverState::Discoverable);
    let s = d.status_at(75_001);
    assert_eq!(s.state, DiscoverState::Stale, "a stuck read");
    assert_eq!(s.degraded_reason(), None);
    assert_eq!(s.seen_at_ms, Some(UTC + 40_000), "the rest is kept");
    // The next read ends it.
    d.observe(seen("10.0.0.1:5961"), at(80_000));
    assert_eq!(d.status_at(80_000).state, DiscoverState::Discoverable);
}

#[test]
fn an_unavailable_check_judges_nothing_and_never_goes_stale() {
    let mut d = begun(0);
    d.unavailable("no NDI source finder (no NDI SDK)", at(7));
    let s = d.status_at(7);
    assert_eq!(s.state, DiscoverState::Unavailable);
    assert_eq!(s.since_ms, Some(UTC + 7));
    assert_eq!(
        s.reason.as_deref(),
        Some("no NDI source finder (no NDI SDK)")
    );
    assert_eq!(s.expected, None);
    assert_eq!(s.degraded_reason(), None);
    for t in [40_000, 50_000, 60_000] {
        assert_eq!(d.observe(None, at(t)), DiscoverLog::Nothing);
    }
    assert_eq!(d.status_at(60_000).reads, 0);
    assert_eq!(d.status_at(1_000_000).state, DiscoverState::Unavailable);
}

#[test]
fn the_state_serializes_in_snake_case() {
    let json = |state| serde_json::to_value(state).unwrap();
    assert_eq!(json(DiscoverState::Starting), "starting");
    assert_eq!(json(DiscoverState::Discoverable), "discoverable");
    assert_eq!(json(DiscoverState::NotDiscoverable), "not_discoverable");
    assert_eq!(json(DiscoverState::Unavailable), "unavailable");
    assert_eq!(json(DiscoverState::Stale), "stale");
}

#[test]
fn a_poll_reads_the_one_finder_and_judges_its_list() {
    let finder = MockNdiSourceFinder::default();
    let discover = NdiDiscover::default();
    let (log, status) = discover.poll(&finder, &dante(), || at(1_000));
    assert_eq!(log, DiscoverLog::Nothing);
    assert_eq!(status, DiscoverStatus::default());
    assert!(finder.polls().is_empty(), "no read before the check began");

    discover.begin(NAME.into(), at(1_000));
    finder.set_sources(vec![
        src("X (y)", "1.2.3.4:5"),
        src(NAME, "10.77.7.107:5961"),
    ]);
    let (log, status) = discover.poll(&finder, &dante(), || at(11_000));
    assert_eq!(log, DiscoverLog::FirstSeen);
    assert_eq!(status.state, DiscoverState::Discoverable);
    assert!(status.on_dante, "the Dante subnets are applied");
    assert_eq!(status, discover.status_at(11_000));
    assert_eq!(finder.polls(), vec![DISCOVER_WAIT_MS]);
    assert_eq!(DISCOVER_WAIT_MS, 1000);

    finder.set_sources(vec![]);
    for t in [21_000, 31_000] {
        assert_eq!(
            discover.poll(&finder, &dante(), || at(t)).0,
            DiscoverLog::Nothing
        );
    }
    let (log, status) = discover.poll(&finder, &dante(), || at(41_000));
    assert_eq!(log, DiscoverLog::Lost);
    assert_eq!(status.state, DiscoverState::NotDiscoverable);
    assert_eq!(finder.polls().len(), 4);
    // The status a poll answers is judged at its own instant: a read after
    // a long stall is fresh, never stale.
    let (_, status) = discover.poll(&finder, &dante(), || at(500_000));
    assert_eq!(status.state, DiscoverState::NotDiscoverable);

    discover.unavailable("gone", at(500_001));
    assert_eq!(
        discover.status_at(500_001).state,
        DiscoverState::Unavailable
    );
    assert_eq!(
        discover.poll(&finder, &dante(), || at(600_000)).0,
        DiscoverLog::Nothing
    );
    assert_eq!(finder.polls().len(), 5, "no read once unavailable");
    assert_eq!(
        discover.status_at(10_000_000).state,
        DiscoverState::Unavailable,
        "an unavailable check never reads stale, whatever it read before"
    );
}
