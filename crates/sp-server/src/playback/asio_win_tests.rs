//! #233: the ASIO glue on the Windows job (`cargo test --workspace` on
//! windows-latest; the runner has no ASIO driver, the boxes several — every
//! assert holds on both): the driver list never fails and an unknown driver
//! is not found with the list; the buffer switch writes L/R into the two
//! configured channels in the driver's type, zeroes every other channel and
//! the frames the ring did not deliver, and only counts; every slot's four
//! callbacks reach that slot; a driver message is counted and answered (a
//! 0 Hz report too); a device releases its slot; a device holds its
//! thread's COM apartment for its whole life; a held driver is refused
//! before the registry is read; a callback stuck past the bound parks the
//! device (its slot, driver and hold), and the output replacing it is told
//! so; `close` pumps the thread's messages while a callback finishes.

use std::sync::{Mutex, MutexGuard};

use super::*;
use sp_core::audio_outputs::MAX_ASIO_OUTPUTS;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Foundation::{S_FALSE, S_OK};
use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows_sys::Win32::UI::WindowsAndMessaging::{KillTimer, SetTimer};

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

/// A driver another output of this process holds (a rebuilt entry's
/// predecessor still releasing it) is refused as held before anything is
/// loaded, even before the registry is read; once given back, the open goes
/// on (here: to the registry, which does not list the test name).
#[test]
fn a_device_does_not_load_a_driver_another_output_holds() {
    let name = "Held Card (songplayer test)";
    let held = HELD.claim(name).expect("no other test holds it");
    let mut d = WinAsioDevice::new();
    assert_eq!(d.open(name, [0, 1]), Err(Reason::Held));
    drop(held);
    assert!(
        matches!(d.open(name, [0, 1]), Err(Reason::NotFound { .. })),
        "given back: the open reads the registry"
    );
    assert!(!HELD.is_held(name), "a failed open holds nothing");
}

/// A callback that never returned: the test's own "driver thread", given
/// back (and its stream freed) when the test ends, however it ends.
struct StuckCallback {
    slot: usize,
    stream: *mut Stream,
}

impl Drop for StuckCallback {
    fn drop(&mut self) {
        SLOTS[self.slot].in_flight.fetch_sub(1, Ordering::SeqCst);
        release(self.slot);
        // SAFETY: the device never frees a stream a callback is inside; the
        // slot no longer points to it and the "callback" is done.
        drop(unsafe { Box::from_raw(self.stream) });
    }
}

/// A callback still inside its stream after the 1 s bound PARKS everything
/// it may touch (iemmixer `asio.rs:487-500`): the stream (leaked), the
/// buffers it writes (never disposed), the driver (never released), the
/// slot (never reused: its in-flight count is the stuck callback's) and the
/// driver's hold (no other output of the process loads the driver). The
/// device says so on its next open (review round 2).
#[test]
fn a_callback_stuck_past_the_bound_parks_its_slot_driver_and_hold() {
    let _g = serial();
    let i = 3;
    claim(i);
    let name = "Parked Card (songplayer test)";
    let (_p, consumer) = rtrb::RingBuffer::<f32>::new(8);
    let stream = Box::into_raw(Box::new(Stream {
        ring: UnsafeCell::new(consumer),
        scratch: UnsafeCell::new(vec![0.0; 8]),
        buffers: Vec::new(),
        frames: 4,
        sample: AsioSample::Int32In24,
        left: 0,
        right: 1,
    }));
    SLOTS[i].stream.store(stream, Ordering::SeqCst);
    SLOTS[i].in_flight.fetch_add(1, Ordering::SeqCst);
    let _stuck = StuckCallback { slot: i, stream };
    let mut d = WinAsioDevice::new();
    d.slot = Some(i);
    d.stream = stream;
    d.hold = HELD.claim(name);
    d.close();
    assert!(
        SLOTS[i].stream.load(Ordering::SeqCst).is_null(),
        "the stream is unhooked"
    );
    assert!(
        SLOTS[i].claimed.load(Ordering::SeqCst),
        "the slot stays claimed: the stuck callback still counts on it"
    );
    assert_eq!(d.open(name, [0, 1]), Err(Reason::Parked));
    drop(d);
    assert!(
        HELD.is_held(name),
        "the parked driver's hold stays for the process's life"
    );
}

/// A device parked on slot `i` with `name` held (its callback stuck; the
/// returned guard plays the callback's return when dropped).
fn parked_device(i: usize, name: &str) -> (WinAsioDevice, StuckCallback) {
    claim(i);
    let (_p, consumer) = rtrb::RingBuffer::<f32>::new(8);
    let stream = Box::into_raw(Box::new(Stream {
        ring: UnsafeCell::new(consumer),
        scratch: UnsafeCell::new(vec![0.0; 8]),
        buffers: Vec::new(),
        frames: 4,
        sample: AsioSample::Int32In24,
        left: 0,
        right: 1,
    }));
    SLOTS[i].stream.store(stream, Ordering::SeqCst);
    SLOTS[i].in_flight.fetch_add(1, Ordering::SeqCst);
    let stuck = StuckCallback { slot: i, stream };
    let mut d = WinAsioDevice::new();
    d.slot = Some(i);
    d.stream = stream;
    d.hold = HELD.claim(name);
    d.close();
    (d, stuck)
}

/// The output that replaces a parked one (an edited entry on the same
/// driver) is told the driver is parked, not that it is still being
/// released (review round 3).
#[test]
fn a_successor_of_a_parked_device_is_told_the_driver_is_parked() {
    let _g = serial();
    let name = "Parked Card 2 (songplayer test)";
    let (d, _stuck) = parked_device(4, name);
    drop(d);
    let mut successor = WinAsioDevice::new();
    assert_eq!(successor.open(name, [0, 1]), Err(Reason::Parked));
}

/// The slot whose "callback" a thread timer lets leave (`usize::MAX`: none).
static TIMER_SLOT: AtomicUsize = AtomicUsize::new(usize::MAX);

/// The thread timer's proc: the callback in `TIMER_SLOT` leaves. Windows
/// runs it only when the timer's thread dispatches its messages.
unsafe extern "system" fn callback_leaves(_: HWND, _: u32, id: usize, _: u32) {
    let i = TIMER_SLOT.swap(usize::MAX, Ordering::SeqCst);
    if i < ASIO_SLOTS {
        SLOTS[i].in_flight.fetch_sub(1, Ordering::SeqCst);
    }
    // SAFETY: this thread's own timer.
    unsafe { KillTimer(ptr::null_mut(), id) };
}

/// Ends the test's "callback" (when the timer never ran: the device parked
/// and kept the stream, which is freed here) and gives the slot back.
struct TimerCallback {
    slot: usize,
    stream: *mut Stream,
}

impl Drop for TimerCallback {
    fn drop(&mut self) {
        if TIMER_SLOT.swap(usize::MAX, Ordering::SeqCst) == self.slot {
            SLOTS[self.slot].in_flight.fetch_sub(1, Ordering::SeqCst);
            // SAFETY: the parked device never frees it; no callback is in it.
            drop(unsafe { Box::from_raw(self.stream) });
        }
        release(self.slot);
    }
}

/// A driver may need the closing thread's messages to finish a callback
/// (iemmixer `asio.rs:483-497`): `close` pumps them while it waits, so such
/// a callback leaves and the device is NOT parked (review round 5). The
/// "callback" here leaves when a 10 ms thread timer's proc runs — only on a
/// dispatch of the thread's messages.
#[test]
fn close_pumps_the_threads_messages_while_a_callback_finishes() {
    let _g = serial();
    std::thread::spawn(|| {
        let i = 5;
        claim(i);
        let (_p, consumer) = rtrb::RingBuffer::<f32>::new(8);
        let stream = Box::into_raw(Box::new(Stream {
            ring: UnsafeCell::new(consumer),
            scratch: UnsafeCell::new(vec![0.0; 8]),
            buffers: Vec::new(),
            frames: 4,
            sample: AsioSample::Int32In24,
            left: 0,
            right: 1,
        }));
        SLOTS[i].stream.store(stream, Ordering::SeqCst);
        SLOTS[i].in_flight.fetch_add(1, Ordering::SeqCst);
        TIMER_SLOT.store(i, Ordering::SeqCst);
        let _callback = TimerCallback { slot: i, stream };
        // SAFETY: a timer of this thread, killed by its own proc.
        let timer = unsafe { SetTimer(ptr::null_mut(), 0, 10, Some(callback_leaves)) };
        assert_ne!(timer, 0, "SetTimer");
        let mut d = WinAsioDevice::new();
        d.slot = Some(i);
        d.stream = stream;
        d.close();
        assert_eq!(
            SLOTS[i].in_flight.load(Ordering::SeqCst),
            0,
            "the callback left"
        );
        assert!(
            !SLOTS[i].claimed.load(Ordering::SeqCst),
            "not parked: the slot is released"
        );
    })
    .join()
    .expect("the closing thread");
}
