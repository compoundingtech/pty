//! Production Core cost benchmark; intentionally ignored and release-only.
//! cargo test --release -p pty --lib frame_bench::bounded_frame_publication -- --ignored --nocapture
//! cargo test --release -p pty --lib frame_bench::sustained_frame_publication -- --ignored --nocapture

use super::*;
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};

const SAMPLES: usize = 5;
const IMAGE_ID: u32 = 1;
const IMAGE_BYTES: usize = 384 * 192 * 4;

#[derive(Clone, Copy)]
enum Stage {
    WriteOnly,
    Unobserved,
    Coalesced,
    Isolated,
    Idle,
}

impl Stage {
    fn name(self) -> &'static str {
        match self {
            Self::WriteOnly => "write_only",
            Self::Unobserved => "core_unobserved",
            Self::Coalesced => "core_observed_coalesced",
            Self::Isolated => "core_observed_isolated",
            Self::Idle => "core_observed_idle_no_input",
        }
    }
}

fn core(rows: u16, cols: u16, image: bool) -> (Core, Receiver<HandleEvent>) {
    let (tx, _rx) = mpsc::channel();
    let (events_tx, events_rx) = mpsc::channel();
    let shared = Arc::new(Shared {
        state: Mutex::new(State { attempt: 1, ..State::default() }),
        cv: Condvar::new(),
        subs: Mutex::new(vec![events_tx]),
        frame: ArcSwap::from_pointee(Frame::default()),
        observers: AtomicUsize::new(0),
    });
    let mut actor = TerminalActor::new(rows, cols, 10_000);
    if image {
        assert!(actor.enable_graphics(GraphicsOptions::DEFAULT));
        // Real Kitty RGBA transmission, not a synthetic descriptor or pixel cache.
        let encoded = "////".repeat(IMAGE_BYTES / 3);
        let chunks = encoded.as_bytes().chunks(4096);
        let count = chunks.len();
        for (index, chunk) in chunks.enumerate() {
            let more = usize::from(index + 1 != count);
            let header = if index == 0 {
                format!("\x1b_Ga=t,f=32,s=384,v=192,i={IMAGE_ID},q=2,m={more};")
            } else {
                format!("\x1b_Gm={more};")
            };
            actor.write(header.as_bytes());
            actor.write(chunk);
            actor.write(b"\x1b\\");
        }
        actor.write(b"\x1b_Ga=p,U=1,i=1,p=1,c=12,r=4,q=2\x1b\\\x1b[H\x1b[38;2;0;0;1m\x1b[58:2::0:0:1m");
        actor.write("\u{10eeee}\u{305}\u{305}\x1b[39;59m".as_bytes());
        assert_eq!(actor.image_bytes(IMAGE_ID).unwrap().data.len(), IMAGE_BYTES);
        assert_eq!(actor.graphics_state(0).images.len(), 1);
    }
    // Preserve the unchanged placement above the scrolling log region.
    actor.write(format!("\x1b[5;{rows}r\x1b[5;1H").as_bytes());
    let mut core = Core {
        actor,
        attempt: AttemptId(1),
        shared,
        backend: Backend::Attach {
            connector: Box::new(|| Err(io::Error::other("benchmark has no transport"))),
            opts: AttachOptions::default(),
            stream: None,
            tx,
        },
        dirty: false,
        last_publish: Instant::now(),
    };
    core.publish();
    core.flush_frame(true);
    drain(&events_rx);
    (core, events_rx)
}

fn drain(events: &Receiver<HandleEvent>) {
    for event in events.try_iter() {
        black_box(event);
    }
}

fn dispatch(core: &mut Core, bytes: Vec<u8>) {
    assert!(core.dispatch(Msg::Output { attempt: AttemptId(1), bytes }));
}

struct Measurement {
    per_batch_ns: f64,
    whole_group_ns: f64,
    last_write_to_store_ns: Option<f64>,
}

fn measure(stage: Stage, rows: u16, cols: u16, image: bool, bytes: &[u8], tiny: bool) -> Measurement {
    let (mut core, events) = core(rows, cols, image);
    let observed = matches!(stage, Stage::Coalesced | Stage::Isolated | Stage::Idle);
    core.shared.observers.store(usize::from(observed), Ordering::Release);
    let initial = core.shared.frame.load_full();
    let held_image = initial.image_bytes(IMAGE_ID).cloned();
    // Native initialization and warmup are excluded; all capture uses Core itself.
    for _ in 0..16 {
        dispatch(&mut core, bytes.to_vec());
        core.flush_frame(false);
        drain(&events);
    }
    core.flush_frame(true);
    drain(&events);
    let before = core.shared.frame.load_full();
    let before_rev = core.rev();
    let group_size = match stage {
        Stage::Coalesced => if tiny { 1000 } else { 16 },
        _ => 1,
    };
    let groups = match stage {
        Stage::Coalesced => 2,
        Stage::Idle => 10_000,
        _ => if tiny { 256 } else { 32 },
    };
    // Model already-queued owned messages: allocation is outside timing, disposal
    // is inside. No producer or transport cost is attributed to frame publication.
    let inputs: Vec<_> = if matches!(stage, Stage::Idle) {
        Vec::new()
    } else {
        (0..groups * group_size).map(|_| bytes.to_vec()).collect()
    };
    let mut inputs = inputs.into_iter();
    let mut tail_ns = 0.0;
    let start = Instant::now();
    for _ in 0..groups {
        match stage {
            Stage::WriteOnly => {
                black_box(core.actor.write(&inputs.next().unwrap()));
            }
            Stage::Unobserved | Stage::Isolated => {
                dispatch(&mut core, inputs.next().unwrap());
                core.flush_due();
                core.flush_frame(false);
                drain(&events);
            }
            Stage::Coalesced => {
                for _ in 0..group_size - 1 {
                    dispatch(&mut core, inputs.next().unwrap());
                    core.flush_due();
                    drain(&events);
                }
                // Tail includes the final queued write, due check, and queue-drain flush.
                let last_write = Instant::now();
                dispatch(&mut core, inputs.next().unwrap());
                core.flush_due();
                core.flush_frame(false);
                tail_ns += last_write.elapsed().as_secs_f64() * 1e9;
                drain(&events);
            }
            Stage::Idle => core.flush_frame(false),
        }
    }
    let elapsed_ns = start.elapsed().as_secs_f64() * 1e9;
    // Correctness and image-sharing checks stay outside every timed region.
    let published = core.shared.frame.load_full();
    if matches!(stage, Stage::Unobserved | Stage::Idle | Stage::WriteOnly) {
        assert!(Arc::ptr_eq(&before, &published));
    }
    if !matches!(stage, Stage::WriteOnly) {
        let updates = if matches!(stage, Stage::Idle) { 0 } else { groups * group_size };
        assert_eq!(core.rev(), before_rev + updates as u64);
    }
    // Force an explicit production barrier even for the deliberately stale cases.
    let (reply, receive) = mpsc::channel();
    assert!(core.dispatch(Msg::CaptureFrame { offset: 0, reply }));
    let latest = receive.recv().unwrap();
    assert_eq!(latest.rev, core.rev());
    assert_eq!(latest.grid, core.actor.snapshot(0));
    assert_eq!(latest.modes, core.actor.modes());
    assert_eq!(latest.graphics, core.actor.graphics_state(0));
    if let Some(held) = held_image {
        assert_eq!(latest.graphics.image(IMAGE_ID), Some(&held.desc));
        assert!(Arc::ptr_eq(latest.image_bytes(IMAGE_ID).unwrap(), &held));
    }
    if matches!(stage, Stage::Coalesced | Stage::Isolated) {
        assert_eq!(published.rev, latest.rev);
        assert_eq!(published.grid, latest.grid);
    }
    Measurement {
        per_batch_ns: elapsed_ns / (groups * group_size) as f64,
        whole_group_ns: elapsed_ns / groups as f64,
        last_write_to_store_ns: matches!(stage, Stage::Coalesced).then_some(tail_ns / groups as f64),
    }
}

fn stats(mut values: Vec<f64>) -> (f64, f64, f64) {
    values.sort_by(f64::total_cmp);
    (values[values.len() / 2], values[0], values[values.len() - 1])
}

#[test]
#[ignore = "release cost experiment; run after all concurrent edits land"]
fn bounded_frame_publication() {
    assert!(!cfg!(debug_assertions), "run this benchmark with --release");
    let small = b"\x1b[90m2026-10-04T12:34:56Z\x1b[0m \x1b[32mINFO\x1b[0m compile module=terminal frames=128 elapsed=3ms ready=true\r\n";
    assert_eq!(small.len(), 103);
    let mut large = Vec::new();
    while large.len() < 65_536 {
        large.extend_from_slice(small);
    }
    println!("scope=actual_core_dispatch_flush_due_and_queue_drain_flush samples={SAMPLES} subscriber_count=1");
    println!("baseline=write_only_not_original_core; core_unobserved_includes_real_State_publication_events_replies_and_owned_message_disposal");
    println!("excluded=producer_allocation,transport,actor_thread_scheduling,concurrent_readers,ui; coalesced_tail=final_write_through_store_not_wire_latency; coalesced_calls_flush_due_after_every_write_and_can_publish_before_queue_drain");
    println!("cols,rows,bytes,graphics,stage,writes_per_group,median_ns_per_batch,min_ns_per_batch,max_ns_per_batch,median_ns_whole_group,min_ns_whole_group,max_ns_whole_group,tail_median_ns,tail_min_ns,tail_max_ns");
    let stages = [Stage::WriteOnly, Stage::Unobserved, Stage::Coalesced, Stage::Isolated, Stage::Idle];
    for (cols, rows) in [(80, 24), (160, 48)] {
        for (bytes, tiny) in [(small.as_slice(), true), (large.as_slice(), false)] {
            for image in [false, true] {
                let mut measurements: [Vec<Measurement>; 5] = std::array::from_fn(|_| Vec::new());
                for sample in 0..SAMPLES {
                    for step in 0..stages.len() {
                        let index = (sample + step) % stages.len();
                        measurements[index].push(measure(stages[index], rows, cols, image, bytes, tiny));
                    }
                }
                for (index, stage) in stages.iter().enumerate() {
                    let values = &measurements[index];
                    let (median, min, max) = stats(values.iter().map(|v| v.per_batch_ns).collect());
                    let (whole, whole_min, whole_max) = stats(values.iter().map(|v| v.whole_group_ns).collect());
                    let tails: Vec<_> = values.iter().filter_map(|v| v.last_write_to_store_ns).collect();
                    let tail = if tails.is_empty() {
                        "NA,NA,NA".to_owned()
                    } else {
                        let (median, min, max) = stats(tails);
                        format!("{median:.1},{min:.1},{max:.1}")
                    };
                    let group_size = if matches!(stage, Stage::Coalesced) { if tiny { 1000 } else { 16 } } else { 1 };
                    println!("{cols},{rows},{},{},{},{group_size},{median:.1},{min:.1},{max:.1},{whole:.1},{whole_min:.1},{whole_max:.1},{tail}", bytes.len(), if image { "unchanged_rgba_384x192" } else { "disabled" }, stage.name());
                }
            }
        }
    }
}

struct FloodMeasurement {
    seconds: f64,
    writes: u64,
    publications: u64,
    max_pending_age_ms: f64,
}

impl FloodMeasurement {
    fn writes_per_second(&self) -> f64 {
        self.writes as f64 / self.seconds
    }
}

fn measure_flood(observed: bool, image: bool, bytes: &[u8]) -> FloodMeasurement {
    let (mut core, events) = core(48, 160, image);
    core.shared.observers.store(usize::from(observed), Ordering::Release);
    for _ in 0..16 {
        dispatch(&mut core, bytes.to_vec());
        core.flush_due();
        drain(&events);
    }
    core.flush_frame(true);
    drain(&events);
    let before = core.shared.frame.load_full();
    let held_image = before.image_bytes(IMAGE_ID).cloned();
    let before_rev = core.rev();
    let mut writes = 0;
    let mut publications = 0;
    let mut max_pending_age = Duration::ZERO;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        // A message is always available: no queue-drain flush in this loop.
        // Unlike the bounded matrix, this includes allocating the owned input.
        dispatch(&mut core, bytes.to_vec());
        writes += 1;
        let previous_publish = core.last_publish;
        max_pending_age = max_pending_age.max(previous_publish.elapsed());
        core.flush_due();
        if core.last_publish != previous_publish {
            publications += 1;
            // Include capture/store time, not just the deadline check's age.
            max_pending_age = max_pending_age.max(core.last_publish.duration_since(previous_publish));
        }
        drain(&events);
    }
    let seconds = start.elapsed().as_secs_f64();
    let measurement = FloodMeasurement {
        seconds,
        writes,
        publications,
        max_pending_age_ms: max_pending_age.as_secs_f64() * 1000.0,
    };
    // No snapshots, image checks, or explicit final flush in the timed loop.
    assert_eq!(core.rev(), before_rev + writes);
    let published = core.shared.frame.load_full();
    if observed {
        assert!(publications > 0, "observed frames must advance during the flood");
        assert!(published.rev > before.rev);
        assert!(published.rev <= core.rev());
    } else {
        assert_eq!(publications, 0);
        assert!(Arc::ptr_eq(&before, &published));
    }
    let (reply, receive) = mpsc::channel();
    assert!(core.dispatch(Msg::CaptureFrame { offset: 0, reply }));
    let latest = receive.recv().unwrap();
    assert!(Arc::ptr_eq(&latest, &core.shared.frame.load_full()));
    assert_eq!(latest.rev, core.rev());
    assert_eq!(latest.grid, core.actor.snapshot(0));
    assert_eq!(latest.modes, core.actor.modes());
    assert_eq!(latest.graphics, core.actor.graphics_state(0));
    assert!(!core.dirty);
    if let Some(held) = held_image {
        assert_eq!(latest.graphics.image(IMAGE_ID), Some(&held.desc));
        assert!(Arc::ptr_eq(latest.image_bytes(IMAGE_ID).unwrap(), &held));
        assert!(Arc::ptr_eq(before.image_bytes(IMAGE_ID).unwrap(), &held));
    }
    measurement
}

#[test]
#[ignore = "release flood experiment; run after all concurrent edits land"]
fn sustained_frame_publication() {
    assert!(!cfg!(debug_assertions), "run this benchmark with --release");
    const FLOOD_SAMPLES: usize = 3;
    const WRITE_BYTES: usize = 64 * 1024;
    let line = b"\x1b[90m2026-10-04T12:34:56Z\x1b[0m \x1b[32mINFO\x1b[0m flood terminal frame progress\r\n";
    let bytes: Vec<_> = line.iter().copied().cycle().take(WRITE_BYTES).collect();
    println!("scope=actual_core_dispatch_and_flush_due continuously_available_input=true cols=160 rows=48 bytes={WRITE_BYTES} samples={FLOOD_SAMPLES} target_seconds=2 subscriber_count=1");
    println!("included=owned_input_allocation,Core_State_and_event_publication,event_drain,per_write_measurement_clocks; excluded=transport,producer_thread,actor_thread_scheduling,concurrent_readers,ui");
    println!("duration=monotonic_wall_clock_including_last_write_overshoot; publications=count_of_changes_to_Core_last_publish_excluding_warmup_and_final_explicit_flush; rate=publications_per_measured_second");
    println!("max_pending_age=maximum_elapsed_since_actual_Core_last_publish_after_a_write_including_capture_store_interval; unobserved_age_is_deliberately_stale_not_a_deadline; no_hard_latency_bound_under_OS_descheduling_or_long_dispatch");
    println!("graphics,sample,observed,seconds,writes,writes_per_second,MiB_per_second,publications,publications_per_second,max_pending_age_ms");
    for image in [false, true] {
        let graphics = if image { "unchanged_rgba_384x192" } else { "disabled" };
        let mut unobserved = Vec::new();
        let mut observed = Vec::new();
        for sample in 0..FLOOD_SAMPLES {
            // Alternate order to avoid always measuring observation second.
            for step in 0..2 {
                let is_observed = (sample + step) % 2 != 0;
                let result = measure_flood(is_observed, image, &bytes);
                let rate = result.writes_per_second();
                println!("{graphics},{sample},{is_observed},{:.6},{},{rate:.3},{:.3},{},{:.3},{:.3}",
                    result.seconds, result.writes, rate * WRITE_BYTES as f64 / (1024.0 * 1024.0),
                    result.publications, result.publications as f64 / result.seconds, result.max_pending_age_ms);
                if is_observed {
                    observed.push(rate);
                } else {
                    unobserved.push(rate);
                }
            }
        }
        let (base, base_min, base_max) = stats(unobserved);
        let (with_frames, observed_min, observed_max) = stats(observed);
        let delta_percent = (with_frames / base - 1.0) * 100.0;
        println!("summary graphics={graphics} median_unobserved_writes_per_second={base:.3} range={base_min:.3}..{base_max:.3} median_observed_writes_per_second={with_frames:.3} range={observed_min:.3}..{observed_max:.3} observed_throughput_delta_percent={delta_percent:.3}");
    }
}
