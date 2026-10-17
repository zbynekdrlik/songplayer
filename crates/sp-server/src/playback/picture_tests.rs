//! Tests for a boundary's picture (#279 P2): either kind, compared by
//! identity, and a GPU picture at a CPU consumer counted, never a panic.

use std::time::Duration;

use sp_gpu::{FakeRing, GpuFrame};

use super::{CPU_MISS_WARN_EVERY_100NS, CpuMisses, CpuUse, Picture};
use crate::playback::frame_buf::SharedFrame;

/// A ring frame of a `width`×`height` picture in `ring`.
fn frame(ring: &FakeRing, width: u32, height: u32) -> GpuFrame {
    ring.frame(width, height, Duration::ZERO)
        .unwrap_or_else(|e| panic!("a free slot: {e}"))
}

#[test]
fn a_cpu_picture_is_its_bytes() {
    let bytes = SharedFrame::new(vec![7u8; 6]);
    let picture = Picture::from(bytes.clone());
    assert_eq!(picture.len(), 6);
    assert!(!picture.is_empty());
    assert!(picture.cpu().is_some_and(|f| f.ptr_eq(&bytes)));
    assert!(picture.gpu().is_none());
    let empty = Picture::Cpu(SharedFrame::new(Vec::new()));
    assert!(empty.is_empty());
}

#[test]
fn a_gpu_picture_holds_no_bytes() {
    let ring = FakeRing::new(1);
    let frame = frame(&ring, 1920, 1080);
    let picture = Picture::from(frame.clone());
    assert_eq!(picture.len(), 0);
    assert!(picture.is_empty());
    assert!(picture.cpu().is_none());
    assert!(picture.gpu().is_some_and(|f| f.ptr_eq(&frame)));
}

#[test]
fn the_same_picture_is_a_clone_of_the_same_allocation_or_frame() {
    let ring = FakeRing::new(2);
    let a = Picture::Cpu(SharedFrame::new(vec![1u8; 4]));
    let same_bytes = Picture::Cpu(SharedFrame::new(vec![1u8; 4]));
    let g = Picture::Gpu(frame(&ring, 64, 64));
    let other_g = Picture::Gpu(frame(&ring, 64, 64));
    assert!(a.ptr_eq(&a.clone()));
    assert!(!a.ptr_eq(&same_bytes), "equal bytes, another allocation");
    assert!(g.ptr_eq(&g.clone()));
    assert!(!g.ptr_eq(&other_g), "another slot");
    assert!(!a.ptr_eq(&g));
    assert!(!g.ptr_eq(&a));
}

#[test]
fn a_gpu_picture_at_a_cpu_consumer_is_a_counted_miss() {
    let ring = FakeRing::new(1);
    let gpu = Picture::Gpu(frame(&ring, 1280, 720));
    let cpu = Picture::Cpu(SharedFrame::new(vec![3u8; 12]));
    let mut misses = CpuMisses::default();
    assert!(
        cpu.cpu_for(CpuUse::Canvas, &mut misses)
            .is_some_and(|f| f.len() == 12)
    );
    assert_eq!(misses.count(), 0, "a CPU picture is no miss");
    assert!(gpu.cpu_for(CpuUse::Canvas, &mut misses).is_none());
    assert!(gpu.cpu_for(CpuUse::Canvas, &mut misses).is_none());
    assert_eq!(misses.count(), 2);
}

#[test]
fn a_consumer_s_misses_are_warned_at_most_once_a_minute() {
    let ring = FakeRing::new(1);
    let frame = frame(&ring, 64, 64);
    let minute = CPU_MISS_WARN_EVERY_100NS;
    assert_eq!(minute, 600_000_000, "60 s");
    let mut misses = CpuMisses::default();
    assert!(misses.note(CpuUse::Preview, &frame, 0), "the first: warned");
    assert!(!misses.note(CpuUse::Preview, &frame, 1));
    assert!(!misses.note(CpuUse::Preview, &frame, minute - 1));
    assert!(
        misses.note(CpuUse::Preview, &frame, minute),
        "a minute after the last WARN"
    );
    assert!(!misses.note(CpuUse::Preview, &frame, minute + 1));
    assert_eq!(misses.count(), 5);
    let mut other = CpuMisses::default();
    assert!(
        other.note(CpuUse::Canvas, &frame, 1),
        "each consumer warns on its own"
    );
}

#[test]
fn each_consumer_says_what_it_shows_of_a_gpu_picture() {
    let (canvas, preview) = (CpuUse::Canvas.shows(), CpuUse::Preview.shows());
    assert!(canvas.contains("black"), "{canvas}");
    assert!(preview.contains("audio"), "{preview}");
    assert_ne!(canvas, preview);
}

#[test]
fn a_picture_prints_its_kind_and_size_never_its_pixels() {
    let ring = FakeRing::new(1);
    let cpu = format!("{:?}", Picture::Cpu(SharedFrame::new(vec![0u8; 42])));
    assert!(cpu.starts_with("Cpu(") && cpu.contains("42"), "{cpu}");
    let gpu = format!("{:?}", Picture::Gpu(frame(&ring, 640, 360)));
    assert!(
        gpu.starts_with("Gpu(") && gpu.contains("640") && gpu.contains("360"),
        "{gpu}"
    );
}

/// A pipeline's ring holds the decode queue's pictures and 8 more for the
/// pacer, the handoff, MAX's queue and the compositor (plan §5.4).
#[cfg(windows)]
#[test]
fn a_pipeline_ring_holds_the_decode_queue_and_eight_more() {
    assert_eq!(
        sp_gpu::RING_CAPACITY,
        crate::playback::pipeline_paced::DECODE_QUEUE_BOUND + 8
    );
}
