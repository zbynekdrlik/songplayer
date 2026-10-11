//! #223 S11: the upgrade's decisions and one song's run, with scripted
//! steps over real files and an in-memory database.

use std::collections::VecDeque;
use std::path::Path;

use sp_decoder::{DecodedVideoFrame, DecoderError, MediaStream, PixelFormat, VideoStream};

use super::test_rig::*;
use super::*;

// ---- facts_of -------------------------------------------------------------

fn picture(width: u32, height: u32, timestamp_ms: u64) -> DecodedVideoFrame {
    DecodedVideoFrame {
        data: Vec::new(),
        width,
        height,
        stride: width,
        timestamp_ms,
        pixel_format: PixelFormat::Nv12,
    }
}

type PictureRead = Result<Option<DecodedVideoFrame>, DecoderError>;

/// A video whose reads, length, rate and seek answer are scripted; it
/// records where it was asked to seek.
struct Scripted {
    reads: VecDeque<PictureRead>,
    duration_ms: u64,
    rate: (u32, u32),
    seek_fails: bool,
    seeks: Vec<u64>,
}

impl Scripted {
    fn new(reads: Vec<PictureRead>, duration_ms: u64, rate: (u32, u32)) -> Self {
        Self {
            reads: reads.into(),
            duration_ms,
            rate,
            seek_fails: false,
            seeks: Vec::new(),
        }
    }
}

impl MediaStream for Scripted {
    fn duration_ms(&self) -> u64 {
        self.duration_ms
    }
    fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError> {
        self.seeks.push(position_ms);
        if self.seek_fails {
            Err(DecoderError::Seek("refused".into()))
        } else {
            Ok(())
        }
    }
}

impl VideoStream for Scripted {
    fn next_frame(&mut self) -> PictureRead {
        self.reads.pop_front().unwrap_or(Ok(None))
    }
    fn width(&self) -> u32 {
        0
    }
    fn height(&self) -> u32 {
        0
    }
    fn frame_rate(&self) -> (u32, u32) {
        self.rate
    }
}

#[test]
fn the_facts_are_the_first_picture_the_rate_the_length_and_a_picture_near_the_end() {
    let mut video = Scripted::new(
        vec![
            Ok(Some(picture(3840, 2160, 42))),
            Ok(Some(picture(3840, 2160, 199_000))),
        ],
        201_000,
        (24_000, 1001),
    );
    let facts = facts_of(&mut video).unwrap();
    assert_eq!(
        facts,
        VideoFacts {
            width: 3840,
            height: 2160,
            first_ms: 42,
            duration_ms: 201_000,
            frame_us: 41_708,
            end_decoded: true,
        }
    );
    assert_eq!(video.seeks, [199_000]);
}

#[test]
fn a_short_video_is_read_near_its_start_and_no_rate_reads_zero() {
    let mut video = Scripted::new(vec![Ok(Some(picture(64, 36, 0)))], 1_500, (0, 1));
    let facts = facts_of(&mut video).unwrap();
    assert_eq!(video.seeks, [0]);
    assert_eq!((facts.frame_us, facts.end_decoded), (0, false));
}

#[test]
fn a_rate_of_25_fps_is_a_40_ms_period() {
    let mut video = Scripted::new(vec![Ok(Some(picture(64, 36, 0)))], 10_000, (25, 1));
    assert_eq!(facts_of(&mut video).unwrap().frame_us, 40_000);
}

#[test]
fn a_video_with_no_picture_or_a_failed_read_has_no_facts() {
    let mut empty = Scripted::new(vec![], 10_000, (25, 1));
    assert_eq!(facts_of(&mut empty), Err("no picture".to_string()));
    let mut failed = Scripted::new(
        vec![Err(DecoderError::ReadSample("broken".into()))],
        10_000,
        (25, 1),
    );
    assert!(
        facts_of(&mut failed)
            .unwrap_err()
            .starts_with("the first picture")
    );
    let mut no_seek = Scripted::new(vec![Ok(Some(picture(64, 36, 0)))], 10_000, (25, 1));
    no_seek.seek_fails = true;
    assert!(
        facts_of(&mut no_seek)
            .unwrap_err()
            .starts_with("the seek to 8000 ms")
    );
}

/// A near-end read that fails is no near-end picture, not a failed read of
/// the facts (`verify` refuses such a video).
#[test]
fn a_failed_read_near_the_end_reads_as_not_decoded() {
    let mut video = Scripted::new(
        vec![
            Ok(Some(picture(64, 36, 0))),
            Err(DecoderError::ReadSample("broken".into())),
        ],
        10_000,
        (25, 1),
    );
    assert!(!facts_of(&mut video).unwrap().end_decoded);
}

// ---- better / verify ------------------------------------------------------

#[test]
fn only_more_rows_than_the_cached_video_are_better() {
    let old = facts(1440, 0, 200_000);
    assert!(better(&format_of(Some(2160)), &old));
    assert!(better(&format_of(Some(1441)), &old));
    assert!(!better(&format_of(Some(1440)), &old));
    assert!(!better(&format_of(Some(1080)), &old));
    assert!(!better(&format_of(None), &old));
}

#[test]
fn a_video_that_reads_as_asked_and_matches_the_old_one_passes() {
    let old = facts(1440, 0, 200_000);
    assert_eq!(
        verify(2160, &old, &facts(2160, 0, 200_000), 200_000),
        Ok(())
    );
    // MF's padding: a 1080-row stream may read 1088 rows.
    assert_eq!(
        verify(1080, &facts(720, 0, 1), &facts(1088, 0, 1), 1),
        Ok(())
    );
}

#[test]
fn a_video_of_other_rows_than_asked_is_refused() {
    let old = facts(1440, 0, 200_000);
    for rows in [2159, 2176] {
        let why = verify(2160, &old, &facts(rows, 0, 200_000), 200_000).unwrap_err();
        assert_eq!(why, format!("it reads {rows} rows, not the 2160 asked"));
    }
    assert_eq!(
        verify(2160, &old, &facts(2175, 0, 200_000), 200_000),
        Ok(())
    );
}

#[test]
fn a_video_no_taller_than_the_cached_one_is_refused() {
    let why = verify(1440, &facts(1440, 0, 1_000), &facts(1440, 0, 1_000), 1_000).unwrap_err();
    assert_eq!(why, "its 1440 rows are no more than the cached 1440");
    assert_eq!(
        verify(1440, &facts(1439, 0, 1_000), &facts(1440, 0, 1_000), 1_000),
        Ok(())
    );
}

#[test]
fn a_video_whose_length_is_not_the_audios_is_refused() {
    let old = facts(1440, 0, 200_000);
    for audio in [198_999, 201_001] {
        let why = verify(2160, &old, &facts(2160, 0, 200_000), audio).unwrap_err();
        assert_eq!(why, format!("it is 200000 ms long, the audio {audio} ms"));
    }
    for audio in [199_000, 201_000] {
        assert_eq!(verify(2160, &old, &facts(2160, 0, 200_000), audio), Ok(()));
    }
}

#[test]
fn a_video_whose_end_does_not_decode_is_refused() {
    let new = VideoFacts {
        end_decoded: false,
        ..facts(2160, 0, 200_000)
    };
    assert_eq!(
        verify(2160, &facts(1440, 0, 200_000), &new, 200_000),
        Err("no picture 2000 ms before its end decoded".to_string())
    );
}

/// The first picture may move by one frame period (40 ms at 25 fps), no
/// more, either way.
#[test]
fn a_video_whose_first_picture_moved_more_than_a_frame_is_refused() {
    let old = facts(1440, 100, 200_000);
    for first in [60, 140] {
        assert_eq!(
            verify(2160, &old, &facts(2160, first, 200_000), 200_000),
            Ok(())
        );
    }
    for first in [59, 141] {
        let why = verify(2160, &old, &facts(2160, first, 200_000), 200_000).unwrap_err();
        assert_eq!(
            why,
            format!("its first picture is at {first} ms, the cached one's at 100 ms")
        );
    }
}

// ---- outcome --------------------------------------------------------------

#[test]
fn an_outcome_reads_as_its_state_and_only_a_finished_check_is_settled() {
    assert_eq!(Outcome::Upgraded.state(None), "upgraded");
    assert_eq!(Outcome::NoBetter.state(None), "no_better");
    assert_eq!(Outcome::Busy.state(Some("held")), "busy");
    assert_eq!(
        Outcome::Failed.state(Some("the resolve: x")),
        "failed: the resolve: x"
    );
    assert_eq!(Outcome::Failed.state(None), "failed: unknown");
    assert_eq!(Outcome::Refused.state(Some("short")), "refused: short");
    assert!(Outcome::Upgraded.settled() && Outcome::NoBetter.settled());
    assert!(Outcome::Refused.settled());
    assert!(!Outcome::Busy.settled() && !Outcome::Failed.settled());
    assert_eq!(serde_json::json!(Outcome::NoBetter), "no_better");
}

#[test]
fn the_upgrade_temp_is_named_for_the_startup_sweep() {
    let temp = upgrade_temp(Path::new("/c"), "PySFfTurafA");
    assert_eq!(temp, Path::new("/c/PySFfTurafA_video_upgrade_temp.mp4"));
    let name = temp.file_name().unwrap().to_str().unwrap();
    assert!(crate::downloader::cache::is_download_temp(name), "{name}");
}

// ---- one run --------------------------------------------------------------

#[tokio::test]
async fn a_taller_stream_replaces_the_video_and_is_recorded_on_every_row() {
    let rig = Rig::new().await;
    std::fs::write(rig.temp(), "stale").unwrap();
    let steps = Fake::upgrading();
    let report = rig.run(&steps).await;
    assert_eq!(
        (report.outcome, report.error.clone()),
        (Outcome::Upgraded, None)
    );
    assert_eq!(report.old, Some(facts(1440, 0, 200_000)));
    assert_eq!(report.new, Some(facts(2160, 0, 200_000)));
    assert_eq!(report.resolved, Some(format_of(Some(2160))));
    // The stale temp was gone before the download.
    assert_eq!(
        *steps.downloads.lock().unwrap(),
        [("401".to_string(), false)]
    );
    assert_eq!(read_to_string(&rig.video), "new");
    assert_eq!(read_to_string(&swap::prev_path(&rig.video)), "old");
    assert!(!rig.temp().exists());
    let state = Some("upgraded".to_string());
    assert_eq!(
        rig.checks().await,
        [
            (10, Some(2160), state.clone(), Some(NOW)),
            (11, Some(2160), state, Some(NOW)),
            (12, None, None, None),
        ]
    );
    assert_eq!(
        rig.formats().await,
        [
            (10, Some("401".to_string()), Some(2160)),
            (11, Some("401".to_string()), Some(2160)),
            (12, None, None),
        ]
    );
}

#[tokio::test]
async fn nothing_taller_downloads_nothing_and_settles_the_check() {
    let rig = Rig::new().await;
    let steps = Fake {
        resolved: Ok(format_of(Some(1440))),
        ..Fake::upgrading()
    };
    let report = rig.run(&steps).await;
    assert_eq!((report.outcome, report.new), (Outcome::NoBetter, None));
    assert!(steps.downloads.lock().unwrap().is_empty());
    assert_eq!(read_to_string(&rig.video), "old");
    assert_eq!(
        rig.checks().await[0],
        (10, Some(2160), Some("no_better".to_string()), Some(NOW))
    );
    assert_eq!(rig.formats().await[0], (10, None, None));
}

/// A video that fails the check never replaces the cached one; the temp is
/// removed, and the check is settled at its cap (`refused`, S12: the same
/// stream would fail the same way).
#[tokio::test]
async fn a_video_that_fails_the_check_changes_nothing() {
    let rig = Rig::new().await;
    let steps = Fake {
        audio_ms: 150_000,
        ..Fake::upgrading()
    };
    let report = rig.run(&steps).await;
    let why = "it is 200000 ms long, the audio 150000 ms";
    assert_eq!(
        (report.outcome, report.error.as_deref()),
        (Outcome::Refused, Some(why))
    );
    assert_eq!(read_to_string(&rig.video), "old");
    assert!(!rig.temp().exists());
    assert!(!swap::prev_path(&rig.video).exists());
    assert_eq!(
        rig.checks().await[1],
        (11, Some(2160), Some(format!("refused: {why}")), Some(NOW))
    );
    assert_eq!(rig.formats().await[1], (11, None, None));
}

/// A refused rename (here: the download wrote no temp) is `busy`: nothing
/// changed, checked again later.
#[tokio::test]
async fn a_refused_rename_is_busy_and_changes_nothing() {
    let rig = Rig::new().await;
    let steps = Fake {
        new_bytes: None,
        ..Fake::upgrading()
    };
    let report = rig.run(&steps).await;
    assert_eq!(report.outcome, Outcome::Busy);
    assert!(report.error.unwrap().starts_with("the rename over"));
    assert_eq!(read_to_string(&rig.video), "old");
    assert!(!swap::prev_path(&rig.video).exists());
    assert_eq!(
        rig.checks().await[0],
        (10, None, Some("busy".to_string()), Some(NOW))
    );
}

#[tokio::test]
async fn a_step_that_fails_ends_the_run_naming_it() {
    let cases = [
        (
            Fake {
                old: Err("broken".into()),
                ..Fake::upgrading()
            },
            "the cached video: broken",
        ),
        (
            Fake {
                resolved: Err("ERROR: sign in".into()),
                ..Fake::upgrading()
            },
            "the resolve: ERROR: sign in",
        ),
        (
            Fake {
                new: Err("no picture".into()),
                ..Fake::upgrading()
            },
            "the new video: no picture",
        ),
    ];
    for (steps, why) in cases {
        let rig = Rig::new().await;
        let report = rig.run(&steps).await;
        assert_eq!(
            (report.outcome, report.error.as_deref()),
            (Outcome::Failed, Some(why))
        );
        assert_eq!(read_to_string(&rig.video), "old");
        assert!(!rig.temp().exists(), "{why}");
        assert_eq!(rig.checks().await[0].1, None, "{why}");
    }
}

#[tokio::test]
async fn a_song_that_is_not_downloaded_is_not_upgraded() {
    let rig = Rig::new().await;
    sqlx::query("UPDATE videos SET normalized = 0")
        .execute(&rig.pool)
        .await
        .unwrap();
    let report = rig.run(&Fake::upgrading()).await;
    assert_eq!(
        (report.outcome, report.error.as_deref()),
        (Outcome::Failed, Some("not downloaded"))
    );
    assert_eq!(report.old, None);
}

#[tokio::test]
async fn the_cached_files_are_the_lowest_downloaded_row_of_the_video() {
    let rig = Rig::new().await;
    sqlx::query("UPDATE videos SET normalized = 0 WHERE id = 10")
        .execute(&rig.pool)
        .await
        .unwrap();
    let song = cached(&rig.pool, YT).await.unwrap().unwrap();
    assert_eq!(song.row_id, 11);
    assert_eq!(song.video, rig.video);
    assert!(song.audio.to_str().unwrap().ends_with("_audio.flac"));
    assert_eq!(cached(&rig.pool, "nonenonenon").await.unwrap(), None);
}

/// A later check that does not finish keeps the cap the last finished one
/// recorded.
#[tokio::test]
async fn an_unfinished_check_keeps_the_recorded_cap() {
    let rig = Rig::new().await;
    record(&rig.pool, YT, Some(1440), "no_better", 1)
        .await
        .unwrap();
    record(&rig.pool, YT, None, "busy", 2).await.unwrap();
    assert_eq!(
        rig.checks().await[0],
        (10, Some(1440), Some("busy".to_string()), Some(2))
    );
}
