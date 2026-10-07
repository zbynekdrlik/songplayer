//! #233: the ASIO glue on the Windows job (`cargo test --workspace` on
//! windows-latest; the runner has no ASIO driver, the boxes several — every
//! assert holds on both): the driver list never fails and an unknown driver
//! is not found with the list; the buffer switch writes L/R into the two
//! configured channels in the driver's type, zeroes every other channel and
//! the frames the ring did not deliver, and only counts; every slot's four
//! callbacks reach that slot; a driver message is counted and answered; a
//! device releases its slot; a device holds its thread's COM apartment for
//! its whole life.

use std::sync::{Mutex, MutexGuard};

use super::*;
use sp_core::audio_outputs::MAX_ASIO_OUTPUTS;
use windows_sys::Win32::Foundation::{S_FALSE, S_OK};
use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};

/// The tests that take callback slots run one at a time (the slots are
/// process-wide statics; nothing else in the test binary claims one).
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|p| p.into_inner())
}

fn claim(i: usize) {
    assert!(
        SLOTS[i]
            .claimed
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok(),
        "slot {i} is free"
    );
    SLOTS[i].clear_counters();
}

fn release(i: usize) {
    SLOTS[i].stream.store(ptr::null_mut(), Ordering::SeqCst);
    SLOTS[i].claimed.store(false, Ordering::SeqCst);
}

#[test]
fn listing_never_fails_and_an_unknown_driver_is_not_found_with_the_list() {
    let names = list_drivers();
    let mut d = WinAsioDevice::new();
    match d.open("No Such Card (songplayer test)", [0, 1]) {
        Err(Reason::NotFound { present }) => assert_eq!(present, names),
        other => panic!("{other:?}"),
    }
    d.close();
    d.close(); // idempotent
    assert_eq!(
        d.poll(),
        DeviceEvents::default(),
        "nothing open, nothing counted"
    );
    assert_eq!((d.consumed_frames(), d.underruns()), (0, 0));
}

#[test]
fn two_slots_per_asio_entry_and_a_start_before_an_open_is_refused() {
    assert_eq!(ASIO_SLOTS, 2 * MAX_ASIO_OUTPUTS);
    let mut d = WinAsioDevice::new();
    let (_p, c) = rtrb::RingBuffer::<f32>::new(4);
    assert_eq!(d.start(c), Err(Reason::Failed("start before open".into())));
}

#[test]
fn the_buffer_switch_writes_left_right_zeroes_the_rest_and_only_counts() {
    let _g = serial();
    let i = 0;
    claim(i);
    let frames = 4;
    let bytes = frames * 4; // Int32LSB24: a 32-bit container
    // Three output channels, both halves of each, filled with junk.
    let mut memory: Vec<Vec<u8>> = (0..3).map(|_| vec![0xAA; 2 * bytes]).collect();
    let buffers: Vec<[*mut c_void; 2]> = memory
        .iter_mut()
        .map(|m| {
            let p = m.as_mut_ptr();
            // SAFETY: `bytes` is half of the allocation.
            [p.cast(), unsafe { p.add(bytes) }.cast()]
        })
        .collect();
    let (mut producer, consumer) = rtrb::RingBuffer::<f32>::new(64);
    let stream = Box::into_raw(Box::new(Stream {
        ring: UnsafeCell::new(consumer),
        scratch: UnsafeCell::new(vec![0.0; frames * 2]),
        buffers,
        frames,
        sample: AsioSample::Int32In24,
        left: 2,
        right: 0,
    }));
    SLOTS[i].stream.store(stream, Ordering::SeqCst);
    // Three frames of L 0.5 / R −0.5: the fourth is missing (an underrun).
    let (pushed, _) = producer.push_partial_slice(&[0.5, -0.5, 0.5, -0.5, 0.5, -0.5]);
    assert_eq!(pushed.len(), 6);
    on_buffer(&SLOTS[i], true);
    assert_eq!(
        SLOTS[i].underruns.load(Ordering::SeqCst),
        0,
        "not primed yet"
    );
    SLOTS[i].primed.store(true, Ordering::SeqCst);
    let left = 4_194_304i32.to_le_bytes();
    let right = (-4_194_304i32).to_le_bytes();
    let half = |c: usize| memory[c][bytes..].to_vec();
    assert_eq!(
        half(2),
        [left, left, left, [0; 4]].concat(),
        "channel 3 = L"
    );
    assert_eq!(
        half(0),
        [right, right, right, [0; 4]].concat(),
        "channel 1 = R"
    );
    assert_eq!(half(1), vec![0; bytes], "an unused channel is silent");
    assert!(
        memory.iter().all(|m| m[..bytes].iter().all(|&b| b == 0xAA)),
        "only the half the driver named is written"
    );
    // The ring is dry now: the first half is silence, an underrun.
    on_buffer(&SLOTS[i], false);
    assert!(memory.iter().all(|m| m[..bytes].iter().all(|&b| b == 0)));
    let s = &SLOTS[i];
    assert_eq!(
        (
            s.underruns.load(Ordering::SeqCst),
            s.consumed.load(Ordering::SeqCst),
            s.callbacks.load(Ordering::SeqCst),
            s.in_flight.load(Ordering::SeqCst),
        ),
        (1, 8, 2, 0)
    );
    release(i);
    // SAFETY: the slot no longer points to it and no callback runs.
    drop(unsafe { Box::from_raw(stream) });
}

#[test]
fn every_slot_s_callbacks_reach_that_slot() {
    let _g = serial();
    for i in 0..ASIO_SLOTS {
        claim(i);
    }
    for (i, cb) in CALLBACKS.iter().enumerate() {
        // SAFETY: the callbacks touch only the static slots (no stream).
        unsafe {
            (cb.sample_rate_did_change)(1_000.0 + i as f64);
            assert_eq!(
                (cb.asio_message)(
                    MessageSelector(selector::OVERLOAD),
                    0,
                    ptr::null(),
                    ptr::null()
                ),
                0
            );
            assert_eq!(
                (cb.asio_message)(
                    MessageSelector(selector::ENGINE_VERSION),
                    0,
                    ptr::null(),
                    ptr::null()
                ),
                2
            );
            (cb.buffer_switch)(0, Bool(1));
            let mut time = Time::default();
            let back = (cb.buffer_switch_time_info)(&mut time, 1, Bool(0));
            assert_eq!(back, &mut time as *mut Time);
        }
    }
    for (i, s) in SLOTS.iter().enumerate() {
        assert_eq!(
            f64::from_bits(s.rate_bits.load(Ordering::SeqCst)),
            1_000.0 + i as f64,
            "slot {i}"
        );
        assert_eq!(s.overloads.load(Ordering::SeqCst), 1, "slot {i}");
        assert_eq!(s.in_flight.load(Ordering::SeqCst), 0, "slot {i}");
    }
    for i in 0..ASIO_SLOTS {
        release(i);
    }
}

#[test]
fn a_driver_message_is_counted_answered_and_polled_once() {
    let _g = serial();
    let i = 1;
    claim(i);
    assert_eq!(on_message(&SLOTS[i], selector::RESET_REQUEST, 0), 1);
    assert_eq!(on_message(&SLOTS[i], selector::BUFFER_SIZE_CHANGE, 256), 0);
    assert_eq!(on_message(&SLOTS[i], selector::RESYNC_REQUEST, 0), 1);
    assert_eq!(on_message(&SLOTS[i], selector::LATENCIES_CHANGED, 0), 1);
    on_message(&SLOTS[i], selector::OVERLOAD, 0);
    // SAFETY: the callback touches only the static slot.
    unsafe { (CALLBACKS[i].sample_rate_did_change)(48_000.0) };
    // A device that holds the slot (it implements Drop: no struct update).
    let mut d = WinAsioDevice::new();
    d.slot = Some(i);
    assert_eq!(
        d.poll(),
        DeviceEvents {
            reset: true,
            resync: true,
            buffer_size_change: true,
            latencies_changed: true,
            rate_changed: Some(48_000.0),
            overloads: 1,
            callbacks: 0,
        }
    );
    let again = d.poll();
    assert_eq!(
        (again.reset, again.rate_changed, again.overloads),
        (false, None, 1),
        "a flag is taken once; the overload count stays"
    );
    d.close();
    assert!(
        !SLOTS[i].claimed.load(Ordering::SeqCst),
        "a closed device releases its slot"
    );
}

/// azo 0.4.0's `SafeHandle` uninitialises COM in its own `Drop`, BEFORE its
/// interface field is released, so with nothing else holding the thread's
/// apartment the driver's `Release` would run after COM is down (review
/// round 1). The device holds its thread's STA itself, from `new` until it
/// is dropped: every driver release (inside `close`, which the device's own
/// `Drop` runs before its fields drop) happens with COM up.
#[test]
fn a_device_holds_its_threads_com_apartment_until_it_is_dropped() {
    std::thread::spawn(|| {
        // A second STA init answers S_FALSE while the thread already is one.
        let probe = || {
            // SAFETY: a plain COM init of this thread, undone at once.
            let hr = unsafe { CoInitializeEx(ptr::null(), COINIT_APARTMENTTHREADED as u32) };
            if hr >= 0 {
                // SAFETY: balances the probe's own successful init.
                unsafe { CoUninitialize() };
            }
            hr
        };
        let device = WinAsioDevice::new();
        assert_eq!(probe(), S_FALSE, "the device holds the thread's STA");
        drop(device);
        assert_eq!(probe(), S_OK, "and gives it back when it is dropped");
    })
    .join()
    .expect("the probe thread");
}

/// `sampleRateDidChange(0.0)` is a lost clock: it reaches the worker as a
/// rate change to 0 Hz (`Reason::RateChanged(0)`), taken once (review round
/// 1: 0 was the "no change" mark of the rate bits, and 0.0's bits are 0).
#[test]
fn a_lost_clock_reported_as_0_hz_reaches_the_worker_once() {
    let _g = serial();
    let i = 2;
    claim(i);
    // SAFETY: the callback touches only the static slot.
    unsafe { (CALLBACKS[i].sample_rate_did_change)(0.0) };
    let mut d = WinAsioDevice::new();
    d.slot = Some(i);
    assert_eq!(d.poll().rate_changed, Some(0.0));
    assert_eq!(d.poll().rate_changed, None, "taken once");
    d.close();
}
