//! Tests for the Spout locks a thread can be handed abandoned (#243).

use super::{
    ACTIVE_SENDER_MAP, MAX_RELEASES, access_mutex_name, holder_text, releases_owed,
    sender_lock_names,
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
