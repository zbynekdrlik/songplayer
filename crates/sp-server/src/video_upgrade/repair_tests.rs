//! #277: the repair of a video Media Foundation stops on, with scripted
//! steps over real temp files and an in-memory DB (the shared `test_rig`).

use std::ffi::OsString;
use std::path::Path;
use std::sync::Mutex;

use super::super::test_rig::{Fake, NOW, Rig, YT, facts, read_to_string};
use super::super::{NO_PICTURE, VideoFacts, swap, verify_repair};
use super::*;

/// The repair of the rig's song with `steps`.
async fn run(rig: &Rig, steps: &Fake) -> RepairReport {
    repair(&rig.pool, rig.dir.path(), YT, steps, NOW).await
}

fn temp(rig: &Rig) -> std::path::PathBuf {
    repair_temp(rig.dir.path(), YT)
}

// ---- the pure parts -------------------------------------------------------

#[test]
fn the_remux_copies_the_video_and_drops_its_sei_for_h264_only() {
    let args = remux_args("H264", Path::new("/c/in.mp4"), Path::new("/c/out.mp4"))
        .expect("H.264 is remuxed");
    let expected: Vec<OsString> = [
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-y",
        "-i",
        "/c/in.mp4",
        "-map",
        "0:v:0",
        "-c",
        "copy",
        "-bsf:v",
        "filter_units=remove_types=6",
        "-movflags",
        "+faststart",
        "/c/out.mp4",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    assert_eq!(args, expected);
    assert!(
        !args
            .iter()
            .any(|a| a == "-c:v" || a == "libx264" || a == "-crf"),
        "no re-encode"
    );
    assert!(
        remux_args("h264", Path::new("a"), Path::new("b")).is_some(),
        "the name's case does not matter"
    );
    for other in ["AV01", "VP90", "HEVC", ""] {
        assert_eq!(
            remux_args(other, Path::new("a"), Path::new("b")),
            None,
            "{other}: its NAL type 6 is no SEI"
        );
    }
}

#[test]
fn a_fresh_file_needs_a_repair_only_when_no_picture_decodes() {
    let plays = facts(1080, 0, 200_000);
    assert!(!has_no_picture(&Ok(plays)));
    let stops = VideoFacts {
        end_decoded: false,
        ..plays
    };
    assert!(
        !has_no_picture(&Ok(stops)),
        "a later stop is the player's to judge, with its 10 s slack"
    );
    assert!(has_no_picture(&Err(NO_PICTURE.to_string())));
    assert!(
        !has_no_picture(&Err("the open: no suitable transform".to_string())),
        "an open that fails is not mended by a remux"
    );
}

#[test]
fn the_repair_temp_is_named_for_the_startup_sweep() {
    let temp = repair_temp(Path::new("/c"), "PySFfTurafA");
    assert_eq!(temp, Path::new("/c/PySFfTurafA_video_repair_temp.mp4"));
    let name = temp.file_name().unwrap().to_str().unwrap();
    assert!(crate::downloader::cache::is_download_temp(name), "{name}");
}

#[test]
fn a_repaired_video_must_play_like_the_cached_one() {
    let old = facts(1080, 0, 200_000);
    let new = facts(1080, 0, 200_000);
    assert_eq!(verify_repair(Some(&old), &new, 200_000), Ok(()));
    assert_eq!(verify_repair(None, &new, 200_000), Ok(()));
    let taller = facts(1088, 0, 200_000);
    assert_eq!(
        verify_repair(Some(&old), &taller, 200_000),
        Err("it reads 1088 rows, the cached video 1080".to_string())
    );
    assert_eq!(
        verify_repair(None, &taller, 200_000),
        Ok(()),
        "a cached video with no picture read has no rows to compare"
    );
    assert_eq!(
        verify_repair(Some(&old), &new, 201_001),
        Err("it is 200000 ms long, the audio 201001 ms".to_string())
    );
    let stops = VideoFacts {
        end_decoded: false,
        ..new
    };
    assert_eq!(
        verify_repair(None, &stops, 200_000),
        Err("no picture 2000 ms before its end decoded".to_string())
    );
    // The first picture: within one frame (40 ms) of the cached one's. With
    // no picture of the cached one read there is nothing to compare: a
    // stream copy moves no timestamp (a stream starting 41 ms in is kept).
    let late = facts(1080, 41, 200_000);
    assert_eq!(
        verify_repair(Some(&old), &late, 200_000),
        Err("its first picture is at 41 ms, the cached one's at 0 ms".to_string())
    );
    assert_eq!(
        verify_repair(Some(&old), &facts(1080, 40, 200_000), 200_000),
        Ok(())
    );
    assert_eq!(verify_repair(None, &late, 200_000), Ok(()));
    let old_late = facts(1080, 41, 200_000);
    assert_eq!(verify_repair(Some(&old_late), &late, 200_000), Ok(()));
}

// ---- one repair -----------------------------------------------------------

/// The observed case: Media Foundation reads no picture of the cached H.264
/// video; the remux reads to its end, so it takes the video's place (one
/// replacing rename), the old file kept as `.prev`, the repair recorded on
/// every row of the video with the cap kept (the song is still checked for
/// an upgrade), and the format untouched.
#[tokio::test]
async fn an_h264_video_is_remuxed_checked_swapped_and_recorded() {
    let rig = Rig::new().await;
    let path = rig.video.to_string_lossy().into_owned();
    let (old_len, old_mtime) = size_and_mtime(&rig.video);
    let entry = crate::db::models_peer::HashEntry {
        path: path.clone(),
        size: old_len,
        mtime_ms: old_mtime,
        sha256: "0123456789abcdef".repeat(4),
        hashed_at_ms: 1,
    };
    crate::db::models_peer::put_hash(&rig.pool, &entry)
        .await
        .unwrap();
    let steps = Fake::repairing();
    let report = run(&rig, &steps).await;
    assert_eq!(
        (report.outcome, report.error.as_deref()),
        (RepairOutcome::Repaired, None)
    );
    assert_eq!(report.codec.as_deref(), Some("H264"));
    assert_eq!(report.old, None, "no picture of the cached video");
    assert_eq!(report.new, Some(facts(1440, 0, 200_000)));
    assert_eq!(read_to_string(&rig.video), "repaired");
    assert_eq!(read_to_string(&swap::prev_path(&rig.video)), "old");
    assert!(!temp(&rig).exists());
    let runs = steps.ffmpeg_runs.lock().unwrap().clone();
    assert_eq!(
        runs,
        vec![remux_args("H264", &rig.video, &temp(&rig)).unwrap()]
    );
    let checks = rig.checks().await;
    assert_eq!(checks[0], (10, None, Some(REPAIRED.to_string()), Some(NOW)));
    assert_eq!(checks[1], (11, None, Some(REPAIRED.to_string()), Some(NOW)));
    assert_eq!(checks[2], (12, None, None, None), "another video");
    assert_eq!(rig.formats().await[0], (10, None, None));
    // The peer hash cache follows the new bytes: the stored entry, made for
    // the old file, no longer holds for the file now under the name, so the
    // hasher's next pass hashes it again (`peer-exchange.md`).
    let stored = crate::db::models_peer::hash_of(&rig.pool, &path)
        .await
        .unwrap()
        .expect("the stored entry");
    let (new_len, new_mtime) = size_and_mtime(&rig.video);
    assert!(stored.holds(old_len, old_mtime), "it held for the old file");
    assert!(
        !stored.holds(new_len, new_mtime),
        "not for the repaired one"
    );
}

/// A file's size and mtime (ms), as the peer hash cache keys them.
fn size_and_mtime(path: &Path) -> (i64, i64) {
    let meta = std::fs::metadata(path).unwrap();
    let mtime = meta
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    (meta.len() as i64, mtime as i64)
}

#[tokio::test]
async fn the_cached_videos_picture_is_the_reference_when_it_reads() {
    let rig = Rig::new().await;
    let steps = Fake {
        old: Ok(VideoFacts {
            end_decoded: false,
            ..facts(1080, 0, 200_000)
        }),
        repaired: Ok(facts(1080, 0, 200_000)),
        ..Fake::upgrading()
    };
    let report = run(&rig, &steps).await;
    assert_eq!(report.outcome, RepairOutcome::Repaired);
    assert_eq!(report.old.map(|old| old.height), Some(1080));

    let rig = Rig::new().await;
    let steps = Fake {
        old: Ok(facts(1080, 0, 200_000)),
        repaired: Ok(facts(1440, 0, 200_000)),
        ..Fake::upgrading()
    };
    let report = run(&rig, &steps).await;
    assert_eq!(
        (report.outcome, report.error.as_deref()),
        (
            RepairOutcome::Refused,
            Some("it reads 1440 rows, the cached video 1080")
        )
    );
    assert_eq!(read_to_string(&rig.video), "old");
}

#[tokio::test]
async fn another_codec_is_unsupported_and_nothing_is_written() {
    let rig = Rig::new().await;
    let steps = Fake {
        codec: Ok("AV01".into()),
        ..Fake::repairing()
    };
    let report = run(&rig, &steps).await;
    assert_eq!(
        (report.outcome, report.error.as_deref()),
        (RepairOutcome::Unsupported, Some("AV01 is not H.264"))
    );
    assert_eq!(report.codec.as_deref(), Some("AV01"));
    assert!(steps.ffmpeg_runs.lock().unwrap().is_empty(), "no ffmpeg");
    assert!(!temp(&rig).exists());
    assert_eq!(read_to_string(&rig.video), "old");
    assert_eq!(rig.checks().await[0], (10, None, None, None));
}

/// A remux that fails the check never replaces the cached video: the temp
/// is removed and nothing is recorded.
#[tokio::test]
async fn a_remux_that_fails_the_check_changes_nothing() {
    let rig = Rig::new().await;
    let steps = Fake {
        repaired: Ok(VideoFacts {
            end_decoded: false,
            ..facts(1440, 0, 200_000)
        }),
        ..Fake::repairing()
    };
    let report = run(&rig, &steps).await;
    assert_eq!(
        (report.outcome, report.error.as_deref()),
        (
            RepairOutcome::Refused,
            Some("no picture 2000 ms before its end decoded")
        )
    );
    assert_eq!(read_to_string(&rig.video), "old");
    assert!(!temp(&rig).exists(), "the remux is deleted");
    assert!(!swap::prev_path(&rig.video).exists());
    assert_eq!(rig.checks().await[0], (10, None, None, None));
}

#[tokio::test]
async fn a_failing_step_ends_the_repair_naming_it() {
    let cases = [
        (
            Fake {
                codec: Err("the open: gone".into()),
                ..Fake::repairing()
            },
            RepairOutcome::Failed,
            "the codec: the open: gone",
        ),
        (
            Fake {
                remux: Err("ffmpeg exited with 1: Invalid data".into()),
                ..Fake::repairing()
            },
            RepairOutcome::Failed,
            "the remux: ffmpeg exited with 1: Invalid data",
        ),
        (
            Fake {
                repaired: Err(NO_PICTURE.into()),
                ..Fake::repairing()
            },
            RepairOutcome::Refused,
            "the repaired video: no picture",
        ),
    ];
    for (steps, outcome, why) in cases {
        let rig = Rig::new().await;
        let report = run(&rig, &steps).await;
        assert_eq!(
            (report.outcome, report.error.as_deref()),
            (outcome, Some(why))
        );
        assert_eq!(read_to_string(&rig.video), "old", "{why}");
        assert!(!temp(&rig).exists(), "{why}");
        assert_eq!(rig.checks().await[0], (10, None, None, None), "{why}");
    }
}

/// A refused rename (here: the remux wrote no file) is `busy`: nothing
/// changed.
#[tokio::test]
async fn a_refused_rename_is_busy_and_changes_nothing() {
    let rig = Rig::new().await;
    let steps = Fake {
        remux: Ok(None),
        ..Fake::repairing()
    };
    let report = run(&rig, &steps).await;
    assert_eq!(report.outcome, RepairOutcome::Busy);
    assert_eq!(read_to_string(&rig.video), "old");
    assert!(!swap::prev_path(&rig.video).exists());
    assert_eq!(rig.checks().await[0], (10, None, None, None));
}

#[tokio::test]
async fn a_video_that_is_not_downloaded_is_not_repaired() {
    let rig = Rig::new().await;
    let report = repair(
        &rig.pool,
        rig.dir.path(),
        "notdownload",
        &Fake::repairing(),
        NOW,
    )
    .await;
    assert_eq!(
        (report.outcome, report.error.as_deref()),
        (RepairOutcome::Failed, Some("not downloaded"))
    );
}

// ---- a fresh download -----------------------------------------------------

/// The download's video and audio in the rig's cache dir.
fn fresh(rig: &Rig) -> (std::path::PathBuf, std::path::PathBuf) {
    let video = rig.dir.path().join("PySFfTurafA_video_temp.mp4");
    std::fs::write(&video, "fresh").unwrap();
    let audio = rig
        .dir
        .path()
        .join("Song_Artist_PySFfTurafA_normalized_audio.flac");
    (video, audio)
}

#[tokio::test]
async fn a_fresh_video_that_plays_is_left_as_it_is() {
    let rig = Rig::new().await;
    let (video, audio) = fresh(&rig);
    let steps = Fake::upgrading();
    let checked = check_download(&steps, YT, &video, &audio, &temp(&rig)).await;
    assert_eq!(checked, Ok(Checked::Plays));
    assert!(steps.ffmpeg_runs.lock().unwrap().is_empty());
    assert_eq!(read_to_string(&video), "fresh");
}

/// #277 (c): a fresh video that ends early is repaired before any row names
/// it: the remux takes its place, so a late-SEI download never reaches a
/// playlist.
#[tokio::test]
async fn a_fresh_video_that_ends_early_is_repaired_in_place() {
    let rig = Rig::new().await;
    let (video, audio) = fresh(&rig);
    let steps = Fake::repairing();
    let checked = check_download(&steps, YT, &video, &audio, &temp(&rig)).await;
    assert_eq!(checked, Ok(Checked::Repaired));
    assert_eq!(read_to_string(&video), "repaired");
    assert!(!temp(&rig).exists());
    assert_eq!(
        steps.ffmpeg_runs.lock().unwrap().clone(),
        vec![remux_args("H264", &video, &temp(&rig)).unwrap()]
    );
    assert_eq!(
        read_to_string(&rig.video),
        "old",
        "the cached song is not touched"
    );
}

#[tokio::test]
async fn a_fresh_video_that_ends_early_and_is_not_repaired_fails_the_download() {
    let rig = Rig::new().await;
    let (video, audio) = fresh(&rig);
    let steps = Fake {
        codec: Ok("VP90".into()),
        ..Fake::repairing()
    };
    let checked = check_download(&steps, YT, &video, &audio, &temp(&rig)).await;
    assert_eq!(
        checked,
        Err(
            "the video plays no picture and was not repaired (Unsupported): VP90 is not H.264"
                .into()
        )
    );
    assert_eq!(read_to_string(&video), "fresh");
    assert!(!temp(&rig).exists());

    let steps = Fake {
        audio_ms: 150_000,
        ..Fake::repairing()
    };
    let checked = check_download(&steps, YT, &video, &audio, &temp(&rig)).await;
    assert_eq!(
        checked,
        Err(
            "the video plays no picture and was not repaired (Refused): \
             it is 200000 ms long, the audio 150000 ms"
                .into()
        )
    );
    assert_eq!(read_to_string(&video), "fresh");
    assert!(!temp(&rig).exists(), "the refused remux is deleted");
}

#[tokio::test]
async fn a_fresh_video_the_reader_cannot_open_is_left_to_the_player() {
    let rig = Rig::new().await;
    let (video, audio) = fresh(&rig);
    let steps = Fake {
        old: Err("the open: no suitable transform".into()),
        ..Fake::upgrading()
    };
    let checked = check_download(&steps, YT, &video, &audio, &temp(&rig)).await;
    assert_eq!(checked, Ok(Checked::Plays));
    assert!(steps.ffmpeg_runs.lock().unwrap().is_empty());

    // A first picture decodes, none 2 s before the end: left to the player.
    let steps = Fake {
        old: Ok(VideoFacts {
            end_decoded: false,
            ..facts(1080, 0, 200_000)
        }),
        ..Fake::upgrading()
    };
    let checked = check_download(&steps, YT, &video, &audio, &temp(&rig)).await;
    assert_eq!(checked, Ok(Checked::Plays));
    assert!(steps.ffmpeg_runs.lock().unwrap().is_empty());
}

// ---- the requests and the worker's step -----------------------------------

#[test]
fn a_video_row_is_asked_for_once_and_taken_oldest_first() {
    let mut requests = Requests::new();
    assert!(requests.ask(3));
    assert!(requests.ask(1));
    assert!(!requests.ask(3), "asked before");
    assert!(requests.is_asked(3));
    assert!(!requests.is_asked(2));
    assert_eq!(requests.take(NOW), Some(3));
    assert_eq!(requests.take(NOW), Some(1));
    assert_eq!(requests.take(NOW), None);
    assert!(!requests.ask(1), "taken, still asked in this process");
    assert!(requests.first_try("abcdefghijk", NOW));
    assert!(!requests.first_try("abcdefghijk", NOW));
}

/// A busy repair is asked again at its due instant, behind the requests
/// queued before it, and its video is tried once more then, once.
#[test]
fn a_busy_repair_waits_for_its_due_instant() {
    let mut requests = Requests::new();
    requests.ask(5);
    requests.ask(6);
    requests.ask(8);
    assert_eq!(requests.take(NOW), Some(5));
    assert!(requests.first_try("abcdefghijk", NOW));
    requests.retry_later(5, "abcdefghijk", NOW + 100);
    assert_eq!(
        requests.take(NOW + 100),
        Some(6),
        "the retry queues behind the requests already there"
    );
    assert_eq!(requests.take(NOW + 99), Some(8), "the due one");
    assert_eq!(requests.take(NOW + 99), None, "not due yet");
    assert!(
        !requests.first_try("abcdefghijk", NOW + 99),
        "no row runs it before the retry"
    );
    assert_eq!(requests.take(NOW + 100), Some(5), "due");
    assert!(requests.first_try("abcdefghijk", NOW + 100), "tried again");
    assert!(
        !requests.first_try("abcdefghijk", NOW + 100),
        "once: the retry is used up"
    );
}

/// One request per tick; the two rows of the song share its file, so the
/// second row's request runs nothing.
#[tokio::test]
async fn the_worker_repairs_a_requested_video_once() {
    let rig = Rig::new().await;
    let requests = Mutex::new(Requests::new());
    requests.lock().unwrap().ask(10);
    requests.lock().unwrap().ask(11);
    let steps = Fake::repairing();
    let first = tick(&rig.pool, rig.dir.path(), &requests, &steps, NOW).await;
    assert_eq!(first.map(|r| r.outcome), Some(RepairOutcome::Repaired));
    assert_eq!(read_to_string(&rig.video), "repaired");
    let second = tick(&rig.pool, rig.dir.path(), &requests, &steps, NOW).await;
    assert_eq!(second, None, "row 11 is the same video, tried already");
    assert_eq!(steps.ffmpeg_runs.lock().unwrap().len(), 1);
    assert_eq!(
        tick(&rig.pool, rig.dir.path(), &requests, &steps, NOW).await,
        None
    );
}

/// #230: a held background keeps the request for the next tick.
#[tokio::test]
async fn a_held_background_keeps_the_request() {
    let rig = Rig::new().await;
    let requests = Mutex::new(Requests::new());
    requests.lock().unwrap().ask(12);
    requests.lock().unwrap().ask(10);
    crate::background_hold::hold_for_a_minute(&rig.pool).await;
    let steps = Fake::repairing();
    let held = tick(&rig.pool, rig.dir.path(), &requests, &steps, NOW).await;
    assert_eq!(held, None);
    assert!(steps.ffmpeg_runs.lock().unwrap().is_empty());
    crate::background_hold::end_hold(&rig.pool).await;
    // The held request is first again: row 12 before row 10 (taken out here,
    // so the tick repairs row 10's video).
    assert_eq!(requests.lock().unwrap().take(NOW), Some(12));
    let after = tick(&rig.pool, rig.dir.path(), &requests, &steps, NOW).await;
    assert_eq!(after.map(|r| r.outcome), Some(RepairOutcome::Repaired));
    assert_eq!(read_to_string(&rig.video), "repaired");
}

/// A player that held the file (`Busy`): the repair is asked again 10 min
/// later and then runs, the one try not used up.
#[tokio::test]
async fn a_busy_repair_runs_again_ten_minutes_later() {
    let rig = Rig::new().await;
    let requests = Mutex::new(Requests::new());
    requests.lock().unwrap().ask(10);
    let busy = Fake {
        remux: Ok(None),
        ..Fake::repairing()
    };
    let first = tick(&rig.pool, rig.dir.path(), &requests, &busy, NOW).await;
    assert_eq!(first.map(|r| r.outcome), Some(RepairOutcome::Busy));
    let steps = Fake::repairing();
    let due = NOW + super::super::worker::BUSY_RETRY_MS;
    let early = tick(&rig.pool, rig.dir.path(), &requests, &steps, due - 1).await;
    assert_eq!(early, None, "not before 10 min");
    let again = tick(&rig.pool, rig.dir.path(), &requests, &steps, due).await;
    assert_eq!(again.map(|r| r.outcome), Some(RepairOutcome::Repaired));
    assert_eq!(read_to_string(&rig.video), "repaired");
}

/// Review round 2: the song's other row asked while the busy retry waits
/// runs nothing (the retry covers the shared file), and once the retry ran
/// a later row's request runs nothing either.
#[tokio::test]
async fn another_row_of_a_busy_video_waits_for_its_one_retry() {
    let rig = Rig::new().await;
    // A third row of the video lives in a third playlist: (playlist_id,
    // youtube_id) is unique, and rows 10 and 11 hold playlists 1 and 2.
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (3, 'c', 'u3')")
        .execute(&rig.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, file_path, \
         audio_file_path) SELECT 13, 3, youtube_id, 1, file_path, audio_file_path \
         FROM videos WHERE id = 10",
    )
    .execute(&rig.pool)
    .await
    .unwrap();
    let requests = Mutex::new(Requests::new());
    requests.lock().unwrap().ask(10);
    requests.lock().unwrap().ask(11);
    let busy = Fake {
        remux: Ok(None),
        ..Fake::repairing()
    };
    let first = tick(&rig.pool, rig.dir.path(), &requests, &busy, NOW).await;
    assert_eq!(first.map(|r| r.outcome), Some(RepairOutcome::Busy));
    let steps = Fake::repairing();
    let minute = tick(&rig.pool, rig.dir.path(), &requests, &steps, NOW + 60_000).await;
    assert_eq!(minute, None, "row 11 waits for the retry");
    assert!(steps.ffmpeg_runs.lock().unwrap().is_empty());
    let due = NOW + super::super::worker::BUSY_RETRY_MS;
    let again = tick(&rig.pool, rig.dir.path(), &requests, &steps, due).await;
    assert_eq!(again.map(|r| r.outcome), Some(RepairOutcome::Repaired));
    requests.lock().unwrap().ask(13);
    let later = tick(&rig.pool, rig.dir.path(), &requests, &steps, due).await;
    assert_eq!(later, None, "the video was repaired once");
    assert_eq!(steps.ffmpeg_runs.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_request_for_a_row_that_is_gone_runs_nothing() {
    let rig = Rig::new().await;
    let requests = Mutex::new(Requests::new());
    requests.lock().unwrap().ask(999);
    let steps = Fake::repairing();
    let ticked = tick(&rig.pool, rig.dir.path(), &requests, &steps, NOW).await;
    assert_eq!(ticked, None);
    assert!(steps.ffmpeg_runs.lock().unwrap().is_empty());
}
