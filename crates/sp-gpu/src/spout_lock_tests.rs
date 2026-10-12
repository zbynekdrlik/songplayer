//! Tests for the Spout locks a thread can be handed abandoned (#243).

use super::{
    ACTIVE_SENDER_MAP, MAX_RELEASES, SENTINEL_WAIT_MS, SentinelStep, access_mutex_name,
    holder_text, program_name, releases_owed, sender_lock_names, sentinel_step,
};

#[test]
fn a_senders_locks_are_the_list_the_active_sender_its_map_and_its_texture() {
    assert_eq!(ACTIVE_SENDER_MAP, "ActiveSenderName");
    assert_eq!(
        access_mutex_name("SP-program-MAX"),
        "SP-program-MAX_SpoutAccessMutex"
    );
    assert_eq!(
        sender_lock_names("SP-program"),
        [
            "SpoutSenderNames_mutex".to_owned(),
            "ActiveSenderName_mutex".to_owned(),
            "SP-program_mutex".to_owned(),
            "SP-program_SpoutAccessMutex".to_owned(),
        ]
    );
}

#[test]
fn a_mutex_this_thread_does_not_own_is_never_released() {
    // Free, owned once and owned three times, all by another thread.
    assert_eq!(releases_owed(1, false), 0);
    assert_eq!(releases_owed(0, false), 0);
    assert_eq!(releases_owed(-2, false), 0);
}

#[test]
fn a_mutex_this_thread_owns_is_released_as_often_as_it_is_held() {
    // NtQueryMutant's count: 0 = owned once, −n = owned n + 1 times.
    assert_eq!(releases_owed(0, true), 1);
    assert_eq!(releases_owed(-1, true), 2);
    assert_eq!(releases_owed(-2, true), 3);
    // A free count with the caller flag set (never seen) releases nothing.
    assert_eq!(releases_owed(1, true), 0);
}

#[test]
fn releases_stop_at_the_bound() {
    assert_eq!(MAX_RELEASES, 64);
    assert_eq!(releases_owed(-62, true), 63);
    assert_eq!(releases_owed(-63, true), 64);
    assert_eq!(releases_owed(-100, true), 64);
    assert_eq!(releases_owed(i32::MIN, true), 64);
}

#[test]
fn a_holder_is_named_by_its_programs_file_name_and_pid() {
    assert_eq!(
        holder_text(
            1500,
            Some(r"C:\Program Files\Resolume Arena\Arena.exe"),
            8492
        ),
        "Arena.exe (pid 1500)"
    );
    assert_eq!(
        holder_text(7360, Some("C:/Program Files/obs-studio/obs64.exe"), 8492),
        "obs64.exe (pid 7360)"
    );
    assert_eq!(holder_text(7, Some("plain.exe"), 8), "plain.exe (pid 7)");
}

#[test]
fn a_holder_whose_image_cannot_be_read_is_named_by_its_pid() {
    assert_eq!(holder_text(1500, None, 8492), "pid 1500");
    assert_eq!(holder_text(1500, Some(""), 8492), "pid 1500");
    assert_eq!(holder_text(1500, Some(r"C:\odd\"), 8492), "pid 1500");
}

#[test]
fn this_process_as_the_holder_says_so() {
    assert_eq!(
        holder_text(
            8492,
            Some(r"C:\Program Files\SongPlayer\SongPlayer.exe"),
            8492
        ),
        "SongPlayer.exe (pid 8492, this process)"
    );
    assert_eq!(holder_text(8492, None, 8492), "pid 8492, this process");
}

#[test]
fn a_program_is_named_by_its_image_s_file_name() {
    assert_eq!(
        program_name(Some(r"C:\Program Files\Resolume Arena\Arena.exe")),
        Some("Arena.exe")
    );
    assert_eq!(program_name(Some("C:/x/obs64.exe")), Some("obs64.exe"));
    assert_eq!(program_name(Some("plain.exe")), Some("plain.exe"));
    assert_eq!(program_name(None), None);
    assert_eq!(program_name(Some("")), None);
    assert_eq!(program_name(Some(r"C:\odd\")), None);
}

/// #243 follow-up: whatever the sentinel is handed it gives back, asked to
/// stop or not: `WAIT_OBJECT_0` (its holder let go) and `WAIT_ABANDONED`
/// (its holder died holding it: the case it exists for).
#[test]
fn the_sentinel_gives_back_a_lock_it_was_handed() {
    for stop in [false, true] {
        assert_eq!(
            sentinel_step(0x0, stop),
            SentinelStep::GiveBack { abandoned: false },
            "stop {stop}"
        );
        assert_eq!(
            sentinel_step(0x80, stop),
            SentinelStep::GiveBack { abandoned: true },
            "stop {stop}: an abandoned lock is given back, never kept"
        );
    }
}

/// #243 follow-up: while the lock stays kept the sentinel waits again, one
/// bounded wait at a time, until it is asked to stop.
#[test]
fn the_sentinel_waits_again_on_a_timeout_until_asked_to_stop() {
    assert_eq!(SENTINEL_WAIT_MS, 500);
    assert_eq!(sentinel_step(0x102, false), SentinelStep::WaitAgain);
    assert_eq!(sentinel_step(0x102, true), SentinelStep::Stop);
}

/// #243 follow-up: any other answer (`WAIT_FAILED`, a second object's
/// index, an I/O completion) ends the sentinel.
#[test]
fn a_failed_wait_ends_the_sentinel() {
    for wait in [0xFFFF_FFFF, 0x1, 0x81, 0xC0, 0x101] {
        assert_eq!(sentinel_step(wait, false), SentinelStep::Fail, "{wait:#x}");
        assert_eq!(sentinel_step(wait, true), SentinelStep::Fail, "{wait:#x}");
    }
}
