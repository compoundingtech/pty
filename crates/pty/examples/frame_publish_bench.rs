//! Isolated per-batch Frame construction/publication cost (not end-to-end latency).
//!
//! Run in the repository's pinned native-library environment:
//! `cargo run --release -p pty --example frame_publish_bench -- 2000 7`
//! Arguments: measured batches per sample, samples (both positive).
//! A real spawned-handle smoke runs before any timers. Timing excludes transport,
//! actor-thread scheduling, Core's shared-state lock/update, event fanout and UI.

use std::error::Error;
use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use pty::{Frame, HandleEvent, SpawnOptions, TerminalHandle};
use pty_terminal::{GraphicsOptions, TerminalActor};

const WARMUP_BATCHES: usize = 256;
const IMAGE_ID: u32 = 1;
const IMAGE_BYTES: usize = 384 * 192 * 4;

#[derive(Clone, Copy)]
enum Stage {
    Write,
    Build,
    Publish,
}

impl Stage {
    fn name(self) -> &'static str {
        match self {
            Self::Write => "write_only",
            Self::Build => "write_frame_arc",
            Self::Publish => "write_frame_arcswap",
        }
    }
}

// Match Core::capture_frame's descriptor-equality fence. Pixels are copied only
// for a new generation; subsequent publications clone the unchanged image Arc.
fn capture(actor: &TerminalActor, previous: &Frame, rev: u64) -> Frame {
    let graphics = actor.graphics_state(0);
    let images = graphics
        .images
        .iter()
        .map(|desc| {
            if let Some(image) = previous.image_bytes(desc.id)
                && image.desc == *desc
            {
                Arc::clone(image)
            } else {
                Arc::new(actor.image_bytes(desc.id).expect("actor owns frame pixels"))
            }
        })
        .collect();
    Frame {
        rev,
        grid: actor.snapshot(0),
        modes: actor.modes(),
        graphics,
        images,
    }
}

fn handle_smoke() -> Result<(), Box<dyn Error>> {
    // Gate output on stdin so subscription cannot miss the child's Dirty event.
    let terminal = TerminalHandle::spawn(
        "sh",
        &["-c", "read -r trigger; printf '\\033[2J\\033[Hframe-smoke\\033[?1006h'"],
        SpawnOptions::default(),
    )?;
    let events = terminal.subscribe();
    let _observer = terminal.observe_frames();
    // Pre-observation lifecycle Dirty events can still be buffered. An explicit
    // initial barrier establishes publication before interpreting that queue.
    let _initial = terminal.request_frame(0).recv_timeout(Duration::from_secs(5))?;
    terminal.write(b"\n");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw_label_dirty = false;
    let mut saw_exit = false;
    loop {
        let event = events.recv_timeout(deadline.saturating_duration_since(Instant::now()))?;
        match event {
            HandleEvent::Dirty(rev) => {
                let frame = terminal.frame();
                assert!(frame.rev >= rev, "Dirty must follow atomic publication");
                let row: String = frame.grid.rows[0].iter().map(|cell| cell.text.as_str()).collect();
                if row.trim_end() == "frame-smoke" {
                    assert!(frame.modes.sgr_mouse, "label and mode must share one frame");
                    saw_label_dirty = true;
                }
            }
            HandleEvent::Exited(code) => {
                assert_eq!(code, 0);
                saw_exit = true;
            }
            _ => {}
        }
        if saw_exit && saw_label_dirty {
            break;
        }
    }
    let frame = terminal.request_frame(0).recv_timeout(Duration::from_secs(5))?;
    let row: String = frame.grid.rows[0].iter().map(|cell| cell.text.as_str()).collect();
    assert!(saw_label_dirty, "subscribed Dirty exposed the child's frame");
    assert_eq!(row.trim_end(), "frame-smoke");
    assert!(frame.modes.sgr_mouse);
    assert_eq!(frame.rev, terminal.rev());
    println!(
        "smoke=ok label=frame-smoke sgr_mouse=true rev={} grid={}x{} exit=0",
        frame.rev, frame.grid.cols, frame.grid.rows_n
    );
    terminal.close();
    Ok(())
}

fn actor(rows: u16, cols: u16, unchanged_image: bool) -> TerminalActor {
    let mut actor = TerminalActor::new(rows, cols, 10_000);
    if unchanged_image {
        assert!(actor.enable_graphics(GraphicsOptions::DEFAULT));
        // Real 384x192 RGBA image, transmitted in Kitty's 4096-byte chunks.
        // Every pixel byte is 0xff; the byte length is divisible by three.
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
        // Keep an actual virtual-placement placeholder outside the scrolling
        // output region, so geometry scanning and image sharing stay exercised.
        actor.write(b"\x1b_Ga=p,U=1,i=1,p=1,c=12,r=4,q=2\x1b\\\x1b[H\x1b[38;2;0;0;1m\x1b[58:2::0:0:1m");
        actor.write("\u{10eeee}\u{305}\u{305}\x1b[39;59m".as_bytes());
        let image = actor.image_bytes(IMAGE_ID).expect("benchmark image accepted");
        assert_eq!(image.data.len(), IMAGE_BYTES);
        assert_eq!(actor.graphics_state(0).images.len(), 1);
    }
    // Identical geometry/scroll margins in the no-graphics control. The image
    // remains in rows 1-4 while logs scroll through the rest of the viewport.
    actor.write(format!("\x1b[5;{rows}r\x1b[5;1H").as_bytes());
    actor
}

fn time_batches(mut batch: impl FnMut(), batches: usize) -> f64 {
    for _ in 0..WARMUP_BATCHES {
        batch();
    }
    let start = Instant::now();
    for _ in 0..batches {
        batch();
    }
    start.elapsed().as_secs_f64() * 1e9 / batches as f64
}

fn measure(stage: Stage, rows: u16, cols: u16, image: bool, bytes: &[u8], batches: usize) -> f64 {
    let mut actor = actor(rows, cols, image);
    // Initial pixel copy, allocation, terminal creation and native page setup
    // are outside the timer. The measured image workload is explicitly steady state.
    let initial = Arc::new(capture(&actor, &Frame::default(), 0));
    let image_arc = initial.image_bytes(IMAGE_ID).cloned();
    let mut previous = initial;
    let published = ArcSwap::from(Arc::clone(&previous));
    let elapsed = match stage {
        Stage::Write => time_batches(|| { black_box(actor.write(black_box(bytes))); }, batches),
        Stage::Build => time_batches(
            || {
                black_box(actor.write(black_box(bytes)));
                previous = Arc::new(capture(&actor, &previous, previous.rev + 1));
                black_box(&previous);
            },
            batches,
        ),
        Stage::Publish => time_batches(
            || {
                black_box(actor.write(black_box(bytes)));
                let previous = published.load();
                let next = Arc::new(capture(&actor, &previous, previous.rev + 1));
                published.store(next);
            },
            batches,
        ),
    };
    if let Some(image_arc) = image_arc {
        let graphics = actor.graphics_state(0);
        assert_eq!(graphics.image(IMAGE_ID), Some(&image_arc.desc));
        let final_frame = match stage {
            Stage::Publish => published.load_full(),
            Stage::Write | Stage::Build => previous,
        };
        assert!(Arc::ptr_eq(final_frame.image_bytes(IMAGE_ID).unwrap(), &image_arc));
    }
    black_box(actor.snapshot(0));
    elapsed
}

fn positive_arg(value: Option<String>, default: usize) -> Result<usize, Box<dyn Error>> {
    let value = value.map(|value| value.parse()).transpose()?.unwrap_or(default);
    if value == 0 {
        return Err("batch/sample counts must be positive".into());
    }
    Ok(value)
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let batches = positive_arg(args.next(), 2000)?;
    let samples = positive_arg(args.next(), 7)?;
    if args.next().is_some() {
        return Err("usage: frame_publish_bench [batches_per_sample] [samples]".into());
    }
    handle_smoke()?;
    println!("scope=isolated_publication_equivalent warmup_batches={WARMUP_BATCHES} measured_batches={batches} samples={samples}");
    println!("excluded=transport,scheduling,core_shared_state_update,event_fanout,ui");
    let small = b"\x1b[90m2026-10-04T12:34:56Z\x1b[0m \x1b[32mINFO\x1b[0m compile module=terminal frames=128 elapsed=3ms ready=true\r\n".to_vec();
    let mut large = Vec::with_capacity(65_536 + small.len());
    while large.len() < 65_536 {
        large.extend_from_slice(&small);
    }
    println!("cols,rows,output,bytes_per_batch,graphics,stage,median_ns_per_batch,min_ns_per_batch,max_ns_per_batch,delta_vs_write_ns,ratio_vs_write");
    let stages = [Stage::Write, Stage::Build, Stage::Publish];
    for (cols, rows) in [(80, 24), (160, 48)] {
        for (output, bytes) in [("small_log", &small), ("large_log_burst", &large)] {
            for image in [false, true] {
                let mut timings: [Vec<f64>; 3] = std::array::from_fn(|_| Vec::with_capacity(samples));
                for sample in 0..samples {
                    // Rotate order to reduce systematic thermal/order bias.
                    for step in 0..stages.len() {
                        let index = (sample + step) % stages.len();
                        timings[index].push(measure(stages[index], rows, cols, image, bytes, batches));
                    }
                }
                for values in &mut timings {
                    values.sort_by(f64::total_cmp);
                }
                let baseline = timings[0][samples / 2];
                for (index, stage) in stages.iter().enumerate() {
                    let values = &timings[index];
                    let median = values[samples / 2];
                    println!(
                        "{cols},{rows},{output},{},{},{},{median:.1},{:.1},{:.1},{:.1},{:.3}",
                        bytes.len(),
                        if image { "unchanged_rgba_384x192" } else { "disabled" },
                        stage.name(),
                        values[0],
                        values[samples - 1],
                        median - baseline,
                        median / baseline,
                    );
                }
            }
        }
    }
    Ok(())
}
