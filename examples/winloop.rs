//! Reproduces main.rs's render loop with synthetic frames and no USB, to tell
//! a minifb problem apart from a capture problem. Auto-exits after 5 s.

use minifb::{Scale, Window, WindowOptions};
use std::time::{Duration, Instant};

const WIDTH: usize = 352;
const HEIGHT: usize = 288;
const IDLE_PUMP: Duration = Duration::from_millis(16);

struct FrameState {
    rgb: Vec<u32>,
    seq: u64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let state: *mut FrameState =
        Box::leak(Box::new(FrameState { rgb: vec![0u32; WIDTH * HEIGHT], seq: 0 }));

    let mut window = Window::new(
        "winloop probe - closes itself after 5s",
        WIDTH,
        HEIGHT,
        WindowOptions { scale: Scale::X2, ..WindowOptions::default() },
    )?;
    window.set_target_fps(0);

    let start = Instant::now();
    let mut last_frame = Instant::now();
    let mut last_pump = Instant::now();
    let mut drawn = 0u64;
    let (mut iters, mut blits, mut pumps) = (0u64, 0u64, 0u64);

    while window.is_open() && start.elapsed() < Duration::from_secs(5) {
        iters += 1;

        // Stand-in for libusb_handle_events_timeout: same 5 ms cadence.
        std::thread::sleep(Duration::from_millis(5));

        // Stand-in for xfer_cb completing a frame at ~10 fps.
        if last_frame.elapsed() >= Duration::from_millis(100) {
            last_frame = Instant::now();
            unsafe {
                let rgb = &mut (*state).rgb;
                let t = start.elapsed().as_secs_f32();
                for y in 0..HEIGHT {
                    for x in 0..WIDTH {
                        let v = (((x as f32 / 20.0 + t * 4.0).sin() * 0.5 + 0.5) * 255.0) as u32;
                        rgb[y * WIDTH + x] = (v << 16) | (v << 8) | v;
                    }
                }
                (*state).seq += 1;
            }
        }

        let seq = unsafe { (*state).seq };
        if seq != drawn {
            drawn = seq;
            blits += 1;
            let rgb = unsafe { &(*state).rgb };
            window.update_with_buffer(rgb, WIDTH, HEIGHT)?;
            last_pump = Instant::now();
        } else if last_pump.elapsed() >= IDLE_PUMP {
            pumps += 1;
            window.update();
            last_pump = Instant::now();
        }
    }

    let s = start.elapsed().as_secs_f32();
    println!("{iters} iters ({:.0}/s), {blits} blits ({:.1}/s), {pumps} idle pumps",
             iters as f32 / s, blits as f32 / s);
    Ok(())
}
