//! Userspace capture for the Sunplus SPCA561A (Rev072A), USB 04fc:0561.
//!
//! Target: Windows + WinUSB (bind with Zadig) via rusb/libusb.
//!
//! Register tables and init ordering are transposed from the Linux kernel
//! driver `drivers/media/usb/gspca/spca561.c`, which is GPL-2.0. This file
//! is therefore a derivative work and is GPL-2.0.
//!
//! Shows a live view in a window. 0-3 switch capture mode, S saves
//! frame_XXXX.ppm, Esc closes it.

use libc::timeval;
use libusb1_sys as ffi;
use minifb::{Key, KeyRepeat, Scale, Window, WindowOptions};
use rusb::{Context, Direction, TransferType, UsbContext};
use std::os::raw::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const VID: u16 = 0x04fc;
const PID: u16 = 0x0561;

/// A Rev072A capture mode. `id` is written to register 0x8500 and indexes
/// CLCK_FOR_MODE; it matches the `priv` field of the kernel driver's mode
/// table. Every mode is raw SGBRG8 Bayer, uncompressed, 1 byte per pixel.
#[derive(Clone, Copy, PartialEq, Eq)]
struct CamMode {
    id: u8,
    w: usize,
    h: usize,
}

const MODES: [CamMode; 4] = [
    CamMode { id: 0, w: 352, h: 288 },
    CamMode { id: 1, w: 320, h: 240 },
    CamMode { id: 2, w: 176, h: 144 },
    CamMode { id: 3, w: 160, h: 120 },
];

impl CamMode {
    fn frame_sz(&self) -> usize {
        self.w * self.h
    }

    /// Isochronous has no retransmission, so lost packets are routine.
    /// Emitting a frame this full beats discarding it: the missing tail keeps
    /// the previous frame's pixels for one frame, which reads as a brief
    /// smear rather than as a halved frame rate.
    fn min_fill(&self) -> usize {
        self.frame_sz() * 7 / 8
    }
}

/// Master clock per mode, from sd_start_72a()
const CLCK_FOR_MODE: [u8; 4] = [0x27, 0x25, 0x22, 0x21];

// A full-speed isochronous packet is one per 1 ms USB frame, so PKTS_PER_XFER
// is also the latency in ms before a completed transfer reaches the callback.
// 8 keeps that granularity low; NUM_TRANSFERS is raised to keep the ring
// covering the same span of time (16 x 8 ms = 128 ms of queued slots).
const NUM_TRANSFERS: usize = 16;
const PKTS_PER_XFER: usize = 8;

/// Cap on how often the message queue is pumped when no new frame arrived.
const IDLE_PUMP: Duration = Duration::from_millis(16);

// ---------------------------------------------------------------------------
// Init tables, verbatim from spca561.c
//
// THE ARGUMENT ORDER TRAP:
//   write_vector tables are  { value, index }
//   sensor tables are        { reg,   value }
// They are opposite. Mirrors the kernel driver exactly.
// ---------------------------------------------------------------------------

const REV72A_RESET: &[[u16; 2]] = &[
    [0x0000, 0x8114],
    [0x0001, 0x8114],
    [0x0000, 0x8112],
];

const REV72A_INIT_DATA1: &[[u16; 2]] = &[
    [0x0003, 0x8701], // PCLK clock delay adjustment
    [0x0001, 0x8703], // HSYNC from cmos inverted
    [0x0011, 0x8118], // enable and conf sensor
    [0x0001, 0x8118], // conf sensor
    [0x0092, 0x8804],
    [0x0010, 0x8802],
];

const REV72A_INIT_SENSOR1: &[[u16; 2]] = &[
    [0x0001, 0x000d],
    [0x0002, 0x0018],
    [0x0004, 0x0165],
    [0x0005, 0x0021],
    [0x0007, 0x00aa],
    [0x0020, 0x1504],
    [0x0039, 0x0002],
    [0x0035, 0x0010],
    [0x0009, 0x1049],
    [0x0028, 0x000b],
    [0x003b, 0x000f],
    [0x003c, 0x0000],
];

const REV72A_INIT_DATA2: &[[u16; 2]] = &[
    [0x0018, 0x8601], // pixel/line selection for color separation
    [0x0000, 0x8602], // optical black level for user setting
    [0x0060, 0x8604], // optical black horizontal offset
    [0x0002, 0x8605], // optical black vertical offset
    [0x0000, 0x8603], // non-automatic optical black level
    [0x0002, 0x865b], // horizontal offset for valid pixels
    [0x0000, 0x865f], // vertical valid pixels window (x2)
    [0x00b0, 0x865d], // horizontal valid pixels window (x2)
    [0x0090, 0x865e], // vertical valid lines window (x2)
    [0x00e0, 0x8406], // memory buffer threshold
    [0x0000, 0x8660], // compensation memory
    [0x0002, 0x8201], // output address for r/w serial EEPROM
    [0x0008, 0x8200], // clear valid bit for serial EEPROM
    [0x0001, 0x8200], // OprMode executed by hardware
    // white balance offsets, lifted from the MS-Win driver
    [0x0000, 0x8611], // R
    [0x00fd, 0x8612], // Gr
    [0x0003, 0x8613], // B
    [0x0000, 0x8614], // Gb
    // white balance gains
    [0x0035, 0x8651], // R
    [0x0040, 0x8652], // Gr
    [0x005f, 0x8653], // B
    [0x0040, 0x8654], // Gb
    [0x0002, 0x8502], // max average bit rate
    [0x0011, 0x8802],
    [0x0087, 0x8700], // master clock
    [0x0081, 0x8702], // master clock output enable
    [0x0000, 0x8500], // image type: 352x288, no compression
    [0x0002, 0x865b], // horizontal offset for valid pixels
    [0x0003, 0x865c], // vertical offset for valid lines
];

const REV72A_INIT_SENSOR2: &[[u16; 2]] = &[
    [0x0003, 0x0121],
    [0x0004, 0x0165],
    [0x0005, 0x002f], // blanking control column
    [0x0006, 0x0000], // blanking mode row
    [0x000a, 0x0002],
    [0x0009, 0x1061], // exposure time && pixel clock
    [0x0035, 0x0014],
];

// ---------------------------------------------------------------------------
// Register access. Every write is the same vendor control transfer:
// bmRequestType 0x40, bRequest 0, wValue = value, wIndex = register.
// ---------------------------------------------------------------------------

struct Cam {
    handle: rusb::DeviceHandle<Context>,
}

impl Cam {
    fn reg_w(&self, index: u16, value: u8) {
        let r = self.handle.write_control(
            0x40, // OUT | VENDOR | DEVICE
            0,    // bRequest
            value as u16,
            index,
            &[],
            Duration::from_millis(500),
        );
        if let Err(e) = r {
            eprintln!("reg_w 0x{index:04x}=0x{value:02x} failed: {e}");
        }
    }

    fn reg_r(&self, index: u16, buf: &mut [u8]) -> bool {
        self.handle
            .read_control(0xC0, 0, 0, index, buf, Duration::from_millis(500))
            .is_ok()
    }

    fn write_vector(&self, table: &[[u16; 2]]) {
        for e in table {
            self.reg_w(e[1], e[0] as u8); // { value, index }
        }
    }

    fn i2c_write(&self, value: u16, reg: u16) {
        self.reg_w(0x8801, reg as u8);
        self.reg_w(0x8805, value as u8);
        self.reg_w(0x8800, (value >> 8) as u8);

        let mut b = [0u8; 1];
        for _ in 0..60 {
            if self.reg_r(0x8803, &mut b) && b[0] == 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        eprintln!("i2c_write reg 0x{reg:02x} timed out");
    }

    fn write_sensor(&self, table: &[[u16; 2]]) {
        for e in table {
            self.i2c_write(e[1], e[0]); // { reg, value }
        }
    }

    fn set_white(&self, white: i32, contrast: i32) {
        let mut red = 0x20 + white * 3 / 8;
        let mut blue = 0x90 - white * 5 / 8;
        red += contrast - 0x20;
        blue += contrast - 0x20;
        self.reg_w(0x8652, (contrast + 0x20) as u8); // Gr
        self.reg_w(0x8654, (contrast + 0x20) as u8); // Gb
        self.reg_w(0x8651, red as u8);
        self.reg_w(0x8653, blue as u8);
    }

    /// Probe-time init, mirroring sd_init_72a. MUST run before start(): the
    /// REV72A_INIT_DATA2 vector carries the valid-pixel window (0x865d = 0xb0
    /// x2 = 352 wide, 0x865e = 0x90 x2 = 288 high), the memory buffer
    /// threshold and the image type. Without them the bridge has no frame
    /// geometry, so it streams pixels continuously and never emits a
    /// start-of-frame (data[0] == 0x00) marker.
    fn init(&self) {
        self.write_vector(REV72A_RESET);
        std::thread::sleep(Duration::from_millis(200));
        self.write_vector(REV72A_INIT_DATA1);
        self.write_sensor(REV72A_INIT_SENSOR1);
        self.write_vector(REV72A_INIT_DATA2);
        self.write_sensor(REV72A_INIT_SENSOR2);
        self.reg_w(0x8112, 0x30);
    }

    fn start(&self, mode: CamMode) {
        self.write_vector(REV72A_RESET);
        std::thread::sleep(Duration::from_millis(200));
        self.write_vector(REV72A_INIT_DATA1);
        self.write_sensor(REV72A_INIT_SENSOR1);

        self.reg_w(0x8700, CLCK_FOR_MODE[mode.id as usize]);
        self.reg_w(0x8702, 0x81);
        self.reg_w(0x8500, mode.id);

        self.write_sensor(REV72A_INIT_SENSOR2);
        self.set_white(0x20, 0x20);

        self.reg_w(0x8112, 0x10 | 0x20); // go
    }

    fn stop(&self) {
        self.reg_w(0x8112, 0x20);
    }
}

// ---------------------------------------------------------------------------
// Frame assembly.
// Each isoc packet starts with a 1-byte sequence number:
//   0x00 -> start of frame, then a 16-byte header to skip (Rev072A)
//   0xff -> empty packet, discard
//   else -> continuation
// ---------------------------------------------------------------------------

struct FrameState {
    mode: CamMode,
    buf: Vec<u8>,
    pos: usize,
    valid: bool,
    /// How many PPMs have been written, i.e. the next frame_NNNN suffix.
    count: u32,
    /// Debayered frame in minifb's 0RGB layout. Reused between frames;
    /// reallocated only when the mode changes.
    rgb: Vec<u32>,
    /// Bumped by emit(). The window loop redraws only when this changes.
    seq: u64,
    /// The frame before `rgb`. Mode 4 interpolates from it towards `rgb`;
    /// only meaningful once `have_prev` is set.
    prev_rgb: Vec<u32>,
    have_prev: bool,
    /// Frames emitted since the last mode change. Interpolating across a
    /// switch would blend two different geometries, so the first frame in a
    /// mode has no predecessor.
    since_mode: u32,
    /// When `rgb` was completed, and a smoothed estimate of the gap between
    /// real frames. Together they put the interpolation phase on the clock.
    frame_at: Instant,
    frame_dt: f64,
    /// Set during teardown so xfer_cb stops resubmitting.
    draining: bool,
    /// Transfers libusb currently owns. Teardown waits for this to reach 0
    /// before freeing them -- freeing a transfer with a callback still
    /// pending is undefined behaviour.
    inflight: usize,
}

impl FrameState {
    fn new(mode: CamMode) -> Self {
        FrameState {
            mode,
            buf: vec![0u8; mode.frame_sz()],
            pos: 0,
            valid: false,
            count: 0,
            rgb: vec![0u32; mode.frame_sz()],
            seq: 0,
            prev_rgb: vec![0u32; mode.frame_sz()],
            have_prev: false,
            since_mode: 0,
            frame_at: Instant::now(),
            frame_dt: 0.1,
            draining: false,
            inflight: 0,
        }
    }

    /// Resize for a new mode and drop any half-assembled frame.
    fn set_mode(&mut self, mode: CamMode) {
        self.mode = mode;
        self.buf = vec![0u8; mode.frame_sz()];
        self.rgb = vec![0u32; mode.frame_sz()];
        self.prev_rgb = vec![0u32; mode.frame_sz()];
        self.have_prev = false;
        self.since_mode = 0;
        self.pos = 0;
        self.valid = false;
    }

    fn packet(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let seq = data[0];
        let mut body = &data[1..];

        if seq == 0xff {
            return;
        }

        if seq == 0x00 {
            if self.valid && self.pos >= self.mode.min_fill() {
                self.emit();
            }
            self.pos = 0;
            self.valid = true;

            if body.len() < 2 {
                self.valid = false;
                return;
            }
            if body[1] & 0x10 != 0 {
                // compressed bayer. Rev072A should never get here.
                eprintln!("unexpected compressed frame");
                self.valid = false;
                return;
            }
            if body.len() < 16 {
                self.valid = false;
                return;
            }
            body = &body[16..]; // skip the Rev072A header
        }

        if !self.valid {
            return;
        }
        let take = body.len().min(self.mode.frame_sz() - self.pos);
        if take == 0 {
            return;
        }
        self.buf[self.pos..self.pos + take].copy_from_slice(&body[..take]);
        self.pos += take;
    }

    fn emit(&mut self) {
        // Retire the frame we were showing so mode 4 can interpolate from it.
        // Swapped rather than copied: the outgoing prev_rgb becomes scratch
        // for the debayer below, which overwrites every pixel of it.
        std::mem::swap(&mut self.rgb, &mut self.prev_rgb);
        self.have_prev = self.since_mode >= 1;

        // Nearest-neighbour GBRG debayer on 2x2 blocks. Crude on purpose.
        // GBRG: row0 = G B G B, row1 = R G R G
        let (w, h) = (self.mode.w, self.mode.h);
        for y in 0..h {
            let yc = y & !1;
            for x in 0..w {
                let xc = x & !1;
                let g = self.buf[yc * w + xc] as u32;
                let b = self.buf[yc * w + xc + 1] as u32;
                let r = self.buf[(yc + 1) * w + xc] as u32;
                self.rgb[y * w + x] = (r << 16) | (g << 8) | b;
            }
        }

        // Measure the real frame interval so interpolation can place itself on
        // the clock. Smoothed, because isochronous delivery is jittery, and
        // clamped so one stalled frame cannot park the phase at an endpoint.
        let now = Instant::now();
        if self.since_mode >= 1 {
            let dt = now.duration_since(self.frame_at).as_secs_f64();
            self.frame_dt = (self.frame_dt * 0.8 + dt * 0.2).clamp(0.005, 1.0);
        }
        self.frame_at = now;
        self.since_mode = self.since_mode.saturating_add(1);
        self.seq = self.seq.wrapping_add(1);
    }

    /// Write the most recent real frame as a binary PPM. Called from the
    /// window loop on a keypress, not from the USB callback. Deliberately not
    /// whatever mode 4 last composed: an interpolated frame is invented, and
    /// a saved still should be something the sensor actually saw.
    fn save_ppm(&mut self) {
        use std::io::Write;
        let name = format!("frame_{:04}.ppm", self.count);
        let mut out = Vec::with_capacity(self.mode.frame_sz() * 3);
        for px in &self.rgb {
            out.push((px >> 16) as u8);
            out.push((px >> 8) as u8);
            out.push(*px as u8);
        }
        match std::fs::File::create(&name) {
            Ok(mut f) => {
                let _ = write!(f, "P6\n{} {}\n255\n", self.mode.w, self.mode.h);
                let _ = f.write_all(&out);
                println!("wrote {name}");
                self.count += 1;
            }
            Err(e) => eprintln!("could not write {name}: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Mode 4: motion-compensated frame interpolation.
//
// Not a capture mode -- the camera keeps running in whichever of 0-3 is
// selected, and this sits on the render side filling the gaps between real
// frames so the window updates at its own rate instead of holding each frame
// until the next one lands.
//
// Block matching, then bidirectional warping. Vectors are estimated once per
// real frame pair and reused for every phase drawn from it, so the per-blit
// cost is only the warp.
//
// Pair this with mode 3: 160x120 already arrives at 25-50 fps, so reaching 60
// is barely more than a doubling. Interpolating mode 0 to 60 means inventing
// six to twelve frames for every real one, which no amount of block matching
// will make look like motion.
// ---------------------------------------------------------------------------

/// Side of the square block motion is estimated over, in pixels.
const BLOCK: usize = 8;
/// Half-width of the search window, in pixels. Motion faster than this is
/// missed, and the block falls back to a straight blend.
const SEARCH: i32 = 6;
/// Mean absolute difference per pixel above which a match is not believed.
/// This sensor is noisy at these sizes, and a confidently wrong vector looks
/// far worse than no vector at all.
const MATCH_LIMIT: i32 = 28;

/// Luma, for matching only. Rec.601 weights in fixed point.
fn luma(p: u32) -> u8 {
    let r = (p >> 16) & 0xff;
    let g = (p >> 8) & 0xff;
    let b = p & 0xff;
    ((r * 77 + g * 150 + b * 29) >> 8) as u8
}

/// Per-channel linear blend. `t` of 0 gives `a`, 1 gives `b`.
fn blend(a: u32, b: u32, t: f32) -> u32 {
    let m = (t.clamp(0.0, 1.0) * 256.0) as u32;
    let n = 256 - m;
    let mix = |sh: u32| ((((a >> sh) & 0xff) * n + ((b >> sh) & 0xff) * m) >> 8) & 0xff;
    (mix(16) << 16) | (mix(8) << 8) | mix(0)
}

/// What mode 4 needs from an interpolator.
///
/// The split matters: `prepare` sees each real frame pair once, `compose` runs
/// per displayed frame. Anything expensive -- motion search, a network forward
/// pass -- belongs in `prepare`, so its cost is paid per captured frame rather
/// than per blit. At 20 fps captured and 60 shown that is a 3x difference.
trait Interpolator {
    /// Called once when a new real frame pair becomes available.
    fn prepare(&mut self, prev: &[u32], cur: &[u32], w: usize, h: usize);

    /// Compose the frame at phase `t`: 0.0 is `prev`, 1.0 is `cur`.
    fn compose(&mut self, prev: &[u32], cur: &[u32], w: usize, h: usize, t: f32) -> &[u32];

    /// Shown at startup and when switching, so it is never a mystery which
    /// engine produced what is on screen.
    fn name(&self) -> &'static str;
}

struct Interp {
    /// Motion from prev to cur, one vector per block, row-major over gw x gh.
    mv: Vec<(i32, i32)>,
    gw: usize,
    gh: usize,
    /// Luma planes for matching, rebuilt once per frame pair.
    lp: Vec<u8>,
    lc: Vec<u8>,
    /// The composed frame handed to the window.
    out: Vec<u32>,
}

impl Interp {
    fn new() -> Self {
        Interp {
            mv: Vec::new(),
            gw: 0,
            gh: 0,
            lp: Vec::new(),
            lc: Vec::new(),
            out: Vec::new(),
        }
    }

    /// Reallocate for a frame size, if it changed.
    fn fit(&mut self, w: usize, h: usize) {
        let gw = (w + BLOCK - 1) / BLOCK;
        let gh = (h + BLOCK - 1) / BLOCK;
        if self.gw == gw && self.gh == gh && self.out.len() == w * h {
            return;
        }
        self.gw = gw;
        self.gh = gh;
        self.mv = vec![(0, 0); gw * gh];
        self.lp = vec![0u8; w * h];
        self.lc = vec![0u8; w * h];
        self.out = vec![0u32; w * h];
    }

    /// Block-match cur against prev. A vector is the motion from prev to cur:
    /// content at Q in prev is at Q + mv in cur.
    fn estimate(&mut self, prev: &[u32], cur: &[u32], w: usize, h: usize) {
        self.fit(w, h);
        for i in 0..w * h {
            self.lp[i] = luma(prev[i]);
            self.lc[i] = luma(cur[i]);
        }

        for by in 0..self.gh {
            for bx in 0..self.gw {
                let x0 = bx * BLOCK;
                let y0 = by * BLOCK;
                let bw = BLOCK.min(w - x0);
                let bh = BLOCK.min(h - y0);

                let mut best = i32::MAX;
                let mut best_mv = (0i32, 0i32);

                for dy in -SEARCH..=SEARCH {
                    for dx in -SEARCH..=SEARCH {
                        let sx = x0 as i32 - dx;
                        let sy = y0 as i32 - dy;
                        if sx < 0
                            || sy < 0
                            || sx + bw as i32 > w as i32
                            || sy + bh as i32 > h as i32
                        {
                            continue;
                        }
                        let mut sad = 0i32;
                        for y in 0..bh {
                            let cr = (y0 + y) * w + x0;
                            let pr = (sy as usize + y) * w + sx as usize;
                            for x in 0..bw {
                                sad += (self.lc[cr + x] as i32 - self.lp[pr + x] as i32).abs();
                            }
                        }
                        // On a tie prefer the shorter vector, so flat or noisy
                        // areas settle on "still" instead of jittering between
                        // equally good matches.
                        let closer = sad == best
                            && dx * dx + dy * dy < best_mv.0 * best_mv.0 + best_mv.1 * best_mv.1;
                        if sad < best || closer {
                            best = sad;
                            best_mv = (dx, dy);
                        }
                    }
                }

                let px = (bw * bh) as i32;
                self.mv[by * self.gw + bx] = if px > 0 && best / px <= MATCH_LIMIT {
                    best_mv
                } else {
                    (0, 0)
                };
            }
        }
    }

    /// Motion at a pixel, bilinear between block centres. Sampling the vector
    /// field smoothly is what keeps block edges from showing up in the warp.
    fn mv_at(&self, x: usize, y: usize) -> (f32, f32) {
        let half = BLOCK as f32 / 2.0;
        let fx = (x as f32 - half) / BLOCK as f32;
        let fy = (y as f32 - half) / BLOCK as f32;
        let (x0, y0) = (fx.floor(), fy.floor());
        let (tx, ty) = (fx - x0, fy - y0);

        let cx = |v: f32| v.clamp(0.0, self.gw as f32 - 1.0) as usize;
        let cy = |v: f32| v.clamp(0.0, self.gh as f32 - 1.0) as usize;
        let (ix0, iy0) = (cx(x0), cy(y0));
        let (ix1, iy1) = (cx(x0 + 1.0), cy(y0 + 1.0));

        let g = |ix: usize, iy: usize| {
            let m = self.mv[iy * self.gw + ix];
            (m.0 as f32, m.1 as f32)
        };
        let (a, b, c, d) = (g(ix0, iy0), g(ix1, iy0), g(ix0, iy1), g(ix1, iy1));
        let top = (a.0 + (b.0 - a.0) * tx, a.1 + (b.1 - a.1) * tx);
        let bot = (c.0 + (d.0 - c.0) * tx, c.1 + (d.1 - c.1) * tx);
        (top.0 + (bot.0 - top.0) * ty, top.1 + (bot.1 - top.1) * ty)
    }

    /// Compose the frame at phase `t` between prev (0.0) and cur (1.0).
    fn render(&mut self, prev: &[u32], cur: &[u32], w: usize, h: usize, t: f32) {
        let (fw, fh) = (w as f32 - 1.0, h as f32 - 1.0);
        for y in 0..h {
            for x in 0..w {
                let (mx, my) = self.mv_at(x, y);
                // Content travelling prev -> cur passes through this pixel at
                // t, so read back along the vector in prev and forward in cur.
                let px = (x as f32 - mx * t).round().clamp(0.0, fw) as usize;
                let py = (y as f32 - my * t).round().clamp(0.0, fh) as usize;
                let qx = (x as f32 + mx * (1.0 - t)).round().clamp(0.0, fw) as usize;
                let qy = (y as f32 + my * (1.0 - t)).round().clamp(0.0, fh) as usize;
                self.out[y * w + x] = blend(prev[py * w + px], cur[qy * w + qx], t);
            }
        }
    }
}

impl Interpolator for Interp {
    fn prepare(&mut self, prev: &[u32], cur: &[u32], w: usize, h: usize) {
        self.estimate(prev, cur, w, h);
    }

    fn compose(&mut self, prev: &[u32], cur: &[u32], w: usize, h: usize, t: f32) -> &[u32] {
        self.render(prev, cur, w, h, t);
        &self.out
    }

    fn name(&self) -> &'static str {
        "block matching"
    }
}

// ---------------------------------------------------------------------------
// RIFE backend, behind the `rife` feature.
//
// vs-mlrt's v1 RIFE exports take one 1x11xHxW float tensor rather than a tidy
// pair of images, because ONNX GridSample wants absolute sampling coordinates
// and the export pushes building them onto the caller:
//
//   0..2   img0 RGB, 0..1
//   3..5   img1 RGB, 0..1
//   6      timestep -- a constant plane holding the phase
//   7      x normalised to -1..1
//   8      y normalised to -1..1
//   9      2/(W-1)
//   10     2/(H-1)
//
// Read off the model with `cargo run --features rife --bin rife_probe`, and
// the channel meanings from vs-mlrt's own wrapper. None of this is documented
// on the model itself.
//
// Both axes must be a multiple of 32 (the pyramid downsamples by that), which
// only 352x288 already satisfies, so frames are padded by edge replication and
// the result cropped back.
// ---------------------------------------------------------------------------

#[cfg(feature = "rife")]
mod rife {
    use super::Interpolator;

    const ALIGN: usize = 32;
    const CHANNELS: usize = 11;

    fn align_up(v: usize) -> usize {
        v.div_ceil(ALIGN) * ALIGN
    }

    pub struct Rife {
        session: ort::session::Session,
        /// Which execution provider actually took the graph. Enabling the
        /// cargo feature only makes DirectML available -- the session still
        /// has to ask for it, and registration can fail back to CPU quietly,
        /// so this records what really happened rather than what we wanted.
        backend: &'static str,
        w: usize,
        h: usize,
        pw: usize,
        ph: usize,
        input: Vec<f32>,
        out: Vec<u32>,
    }

    impl Rife {
        pub fn load(path: &str) -> Result<Self, String> {
            let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;

            // Ask for DirectML explicitly, and treat failure as informative
            // rather than fatal -- CPU still runs this model, just slower.
            // Registration consumes the builder, so the fallback needs a fresh
            // one rather than reusing the moved value.
            let base =
                ort::session::Session::builder().map_err(|e| format!("session builder: {e}"))?;
            let (mut builder, backend) =
                match base.with_execution_providers([ort::ep::DirectML::default().build()]) {
                    Ok(b) => (b, "DirectML"),
                    // ort returns the builder inside the error, so a refused
                    // EP costs nothing: recover it and carry on with CPU.
                    Err(e) => {
                        eprintln!("rife: DirectML unavailable, running on CPU: {}", e.message());
                        (e.recover(), "CPU")
                    }
                };

            let session = builder
                .commit_from_memory(&bytes)
                .map_err(|e| format!("{path}: {e}"))?;
            Ok(Rife {
                session,
                backend,
                w: 0,
                h: 0,
                pw: 0,
                ph: 0,
                input: Vec::new(),
                out: Vec::new(),
            })
        }

        /// Reallocate for a frame size and fill the channels that depend only
        /// on geometry, so they are written once rather than per frame.
        fn fit(&mut self, w: usize, h: usize) {
            if self.w == w && self.h == h {
                return;
            }
            self.w = w;
            self.h = h;
            self.pw = align_up(w);
            self.ph = align_up(h);
            self.input = vec![0.0f32; CHANNELS * self.pw * self.ph];
            self.out = vec![0u32; w * h];

            let (pw, ph) = (self.pw, self.ph);
            let plane = pw * ph;
            let mx = 2.0 / (pw as f32 - 1.0);
            let my = 2.0 / (ph as f32 - 1.0);
            for y in 0..ph {
                for x in 0..pw {
                    let i = y * pw + x;
                    self.input[7 * plane + i] = x as f32 * mx - 1.0;
                    self.input[8 * plane + i] = y as f32 * my - 1.0;
                    self.input[9 * plane + i] = mx;
                    self.input[10 * plane + i] = my;
                }
            }
        }

        /// Write one frame into channels `base..base+3`, edge-replicated into
        /// the padding so the network sees a continued image rather than a
        /// hard black border it would try to interpret as motion.
        fn pack(&mut self, src: &[u32], base: usize) {
            let (w, h, pw, ph) = (self.w, self.h, self.pw, self.ph);
            let plane = pw * ph;
            for y in 0..ph {
                let sy = y.min(h - 1);
                for x in 0..pw {
                    let p = src[sy * w + x.min(w - 1)];
                    let i = y * pw + x;
                    self.input[base * plane + i] = ((p >> 16) & 0xff) as f32 / 255.0;
                    self.input[(base + 1) * plane + i] = ((p >> 8) & 0xff) as f32 / 255.0;
                    self.input[(base + 2) * plane + i] = (p & 0xff) as f32 / 255.0;
                }
            }
        }
    }

    impl Interpolator for Rife {
        fn prepare(&mut self, prev: &[u32], cur: &[u32], w: usize, h: usize) {
            self.fit(w, h);
            self.pack(prev, 0);
            self.pack(cur, 3);
        }

        fn compose(&mut self, _prev: &[u32], _cur: &[u32], w: usize, h: usize, t: f32) -> &[u32] {
            // Unlike block matching there is nothing to reuse across phases:
            // t is an input channel, so the forward pass runs per displayed
            // frame. Only the packing above is saved by prepare().
            let plane = self.pw * self.ph;
            for v in &mut self.input[6 * plane..7 * plane] {
                *v = t;
            }

            let shape = [1i64, CHANNELS as i64, self.ph as i64, self.pw as i64];
            // Any failure here leaves the previous composed frame on screen
            // rather than tearing the stream down: a dropped interpolated
            // frame is not worth killing a working capture over.
            let tensor = match ort::value::Tensor::from_array((shape, self.input.clone())) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("rife: building input failed: {e}");
                    return &self.out;
                }
            };
            let outputs = match self.session.run(ort::inputs!["input" => tensor]) {
                Ok(o) => o,
                Err(e) => {
                    eprintln!("rife: forward pass failed: {e}");
                    return &self.out;
                }
            };
            let data = match outputs["output"].try_extract_tensor::<f32>() {
                Ok((_shape, d)) => d,
                Err(e) => {
                    eprintln!("rife: reading output failed: {e}");
                    return &self.out;
                }
            };

            // Crop the padding back off on the way to 0RGB.
            let pw = self.pw;
            for y in 0..h {
                for x in 0..w {
                    let i = y * pw + x;
                    let c = |v: f32| (v.clamp(0.0, 1.0) * 255.0) as u32;
                    self.out[y * w + x] =
                        (c(data[i]) << 16) | (c(data[plane + i]) << 8) | c(data[2 * plane + i]);
                }
            }
            &self.out
        }

        fn name(&self) -> &'static str {
            match self.backend {
                "DirectML" => "RIFE v4.6 (DirectML)",
                _ => "RIFE v4.6 (CPU)",
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Isochronous ring. Raw FFI, because rusb has no safe isoc API.
// ---------------------------------------------------------------------------

/// libusb declares callbacks with LIBUSB_CALL, which is __stdcall on Windows.
/// If this fails to compile, change to `extern "C"` and check what
/// `libusb1_sys::libusb_transfer_cb_fn` actually resolves to on your target.
extern "system" fn xfer_cb(xfer: *mut ffi::libusb_transfer) {
    unsafe {
        let state = &mut *((*xfer).user_data as *mut FrameState);
        let n = (*xfer).num_iso_packets as usize;
        let base = (*xfer).buffer;

        // iso_packet_desc is a flexible array member at the end of the struct.
        let descs = std::ptr::addr_of!((*xfer).iso_packet_desc)
            as *const ffi::libusb_iso_packet_descriptor;

        // Packet i lives at base + i * (declared packet length), not at
        // a running offset of actual_length. This is the usual isoc gotcha.
        let pkt_len = if n > 0 { (*descs.add(0)).length as usize } else { 0 };

        for i in 0..n {
            let d = &*descs.add(i);
            if d.status != ffi::constants::LIBUSB_TRANSFER_COMPLETED {
                continue;
            }
            if d.actual_length == 0 {
                continue;
            }
            let p = std::slice::from_raw_parts(
                base.add(i * pkt_len),
                d.actual_length as usize,
            );
            state.packet(p);
        }

        // During teardown, let the transfer die instead of resubmitting, and
        // account for it so tear_down_ring knows when libusb is finished
        // with these allocations.
        if state.draining {
            state.inflight -= 1;
            return;
        }
        if ffi::libusb_submit_transfer(xfer) < 0 {
            eprintln!("resubmit failed");
            state.inflight -= 1;
        }
    }
}

/// The submitted isochronous transfers plus the buffers libusb writes into.
/// The buffers must outlive the transfers, so they travel together.
struct Ring {
    xfers: Vec<*mut ffi::libusb_transfer>,
    buffers: Vec<Vec<u8>>,
}

/// Allocate, arm and submit the transfer ring. `state` must stay pinned for as
/// long as the ring lives: each transfer holds it as a raw callback argument.
unsafe fn build_ring(
    dev_handle: *mut ffi::libusb_device_handle,
    ep_addr: u8,
    pkt_size: usize,
    state: *mut FrameState,
) -> Result<Ring, Box<dyn std::error::Error>> {
    let mut ring = Ring { xfers: Vec::new(), buffers: Vec::new() };

    for i in 0..NUM_TRANSFERS {
        let mut buf = vec![0u8; pkt_size * PKTS_PER_XFER];
        let xfer = ffi::libusb_alloc_transfer(PKTS_PER_XFER as i32);
        if xfer.is_null() {
            return Err("libusb_alloc_transfer failed".into());
        }

        (*xfer).dev_handle = dev_handle;
        (*xfer).endpoint = ep_addr;
        (*xfer).transfer_type = ffi::constants::LIBUSB_TRANSFER_TYPE_ISOCHRONOUS;
        (*xfer).timeout = 1000;
        (*xfer).buffer = buf.as_mut_ptr();
        (*xfer).length = (pkt_size * PKTS_PER_XFER) as i32;
        (*xfer).num_iso_packets = PKTS_PER_XFER as i32;
        (*xfer).callback = xfer_cb;
        (*xfer).user_data = state as *mut c_void;
        (*xfer).flags = 0;

        // libusb_set_iso_packet_lengths is a static inline in C, so do it here.
        let descs = std::ptr::addr_of_mut!((*xfer).iso_packet_desc)
            as *mut ffi::libusb_iso_packet_descriptor;
        for p in 0..PKTS_PER_XFER {
            (*descs.add(p)).length = pkt_size as u32;
        }

        let r = ffi::libusb_submit_transfer(xfer);
        if r < 0 {
            ffi::libusb_free_transfer(xfer);
            eprintln!("submit {i} failed: {r}");
            eprintln!(
                "If this is LIBUSB_ERROR_NOT_SUPPORTED (-12), WinUSB isochronous\n\
                 is unavailable. Confirm Windows 8.1+ and that the vendored libusb\n\
                 was built with isoc support."
            );
            return Err("isochronous submit failed".into());
        }
        (*state).inflight += 1;

        ring.buffers.push(buf);
        ring.xfers.push(xfer);
    }
    Ok(ring)
}

/// Cancel every transfer and wait for libusb to hand them all back before
/// freeing anything. Freeing a transfer, or dropping the buffer it points at,
/// while a callback is still pending is undefined behaviour -- so if the
/// drain times out we deliberately leak the ring rather than risk it.
unsafe fn tear_down_ring(
    ctx: &Context,
    state: *mut FrameState,
    ring: Ring,
) -> Result<(), Box<dyn std::error::Error>> {
    (*state).draining = true;
    for x in &ring.xfers {
        ffi::libusb_cancel_transfer(*x);
    }

    let deadline = Instant::now() + Duration::from_secs(2);
    while (*state).inflight > 0 && Instant::now() < deadline {
        let tv = timeval { tv_sec: 0, tv_usec: 10_000 };
        ffi::libusb_handle_events_timeout(ctx.as_raw(), &tv);
    }

    if (*state).inflight > 0 {
        // Give up on the memory rather than free it out from under libusb,
        // and leave `draining` set so any late callback dies instead of
        // resubmitting into a ring we have abandoned. Streaming cannot
        // safely continue from here.
        std::mem::forget(ring);
        return Err(format!(
            "timed out reclaiming {} isochronous transfers",
            (*state).inflight
        )
        .into());
    }

    for x in &ring.xfers {
        ffi::libusb_free_transfer(*x);
    }
    drop(ring);
    (*state).draining = false;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let running = Arc::new(AtomicBool::new(true));
    {
        let r = running.clone();
        ctrlc_shim(move || r.store(false, Ordering::SeqCst));
    }

    let ctx = Context::new()?;
    let handle = ctx
        .open_device_with_vid_pid(VID, PID)
        .ok_or("device 04fc:0561 not found. Did you bind WinUSB with Zadig?")?;

    // Find the isoc IN endpoint with the largest packet size.
    let dev = handle.device();
    let cfg = dev.active_config_descriptor()?;
    let mut best = 0usize;
    let mut best_alt = 0u8;
    let mut ep_addr = 0x81u8;

    for iface in cfg.interfaces() {
        for desc in iface.descriptors() {
            for ep in desc.endpoint_descriptors() {
                if ep.transfer_type() != TransferType::Isochronous {
                    continue;
                }
                if ep.direction() != Direction::In {
                    continue;
                }
                // high-bandwidth: bits 11:12 hold extra transactions per frame
                let raw = ep.max_packet_size();
                let sz = (raw & 0x07ff) as usize * ((((raw >> 11) & 3) + 1) as usize);
                if sz > best {
                    best = sz;
                    best_alt = desc.setting_number();
                    ep_addr = ep.address();
                }
            }
        }
    }
    if best == 0 {
        return Err("no isochronous IN endpoint found".into());
    }
    println!("using alt {best_alt}, ep 0x{ep_addr:02x}, {best} bytes/packet");

    handle.claim_interface(0)?;
    handle.set_alternate_setting(0, best_alt)?;

    let cam = Cam { handle };
    cam.init();

    // Startup overrides, so the thing can be driven without a focused window
    // (scripts, headless checks, or just not wanting to click the preview
    // before pressing a key). Both are equivalent to the keypress.
    let mut mode = MODES[0];
    if let Ok(v) = std::env::var("SPCA_MODE") {
        match v.trim().parse::<usize>() {
            Ok(i) if i < MODES.len() => mode = MODES[i],
            _ => eprintln!("SPCA_MODE must be 0-{}, ignoring {v:?}", MODES.len() - 1),
        }
    }
    cam.start(mode);

    // Deliberately leaked: xfer_cb holds a raw pointer to it for the life of
    // the process. Accessed ONLY through this raw pointer, never through a
    // long-lived &mut -- the callback writes to it from inside
    // libusb_handle_events_timeout, so a &mut held across that call would be
    // aliased, and its noalias would let the compiler hoist reads of .seq out
    // of the render loop and freeze the picture.
    let state: *mut FrameState = Box::leak(Box::new(FrameState::new(mode)));

    let mut ring = unsafe { build_ring(cam.handle.as_raw(), ep_addr, best, state)? };

    // The window is sized for the largest mode. minifb only requires the
    // buffer be big enough and takes the dimensions per call, so the smaller
    // modes are scaled up into the same window without recreating it.
    let mut window = Window::new(
        "SPCA561A live  -  0-3 mode, 4 interpolates, S saves a PPM, Esc quits",
        MODES[0].w,
        MODES[0].h,
        WindowOptions { scale: Scale::X2, ..WindowOptions::default() },
    )?;
    // We pace the loop ourselves off the USB timeout below, so tell minifb
    // not to add any sleep of its own.
    window.set_target_fps(0);

    println!("streaming, Esc or close the window to stop");
    println!("press 0-3 to switch mode:");
    for (i, m) in MODES.iter().enumerate() {
        println!("  {i} = {}x{}", m.w, m.h);
    }
    println!("press 4 to toggle frame interpolation (try it with mode 3)");

    let mut last_pump = Instant::now();
    let mut last_report = Instant::now();
    let mut last_report_seq = 0u64;
    let mut engine: Box<dyn Interpolator> = Box::new(Interp::new());
    // RIFE if the feature is built and a model is on disk, block matching
    // otherwise. A missing model is not an error: mode 4 still works, it just
    // works with the cheaper engine, and the startup line says which.
    #[cfg(feature = "rife")]
    {
        let path = std::env::var("SPCA_RIFE_MODEL")
            .unwrap_or_else(|_| "rife/rife_v4.6.onnx".to_string());
        match rife::Rife::load(&path) {
            Ok(r) => engine = Box::new(r),
            Err(e) => eprintln!("rife unavailable, falling back to block matching: {e}"),
        }
    }
    let mut prepared_for = u64::MAX;
    println!("interpolation engine: {}", engine.name());
    let mut interp_on = matches!(
        std::env::var("SPCA_INTERP").as_deref(),
        Ok("1") | Ok("on") | Ok("true")
    );
    let mut blits = 0u64;
    let mut last_report_blits = 0u64;
    while running.load(Ordering::SeqCst)
        && window.is_open()
        && !window.is_key_down(Key::Escape)
    {
        if last_report.elapsed() >= Duration::from_secs(1) {
            let seq = unsafe { (*state).seq };
            if interp_on {
                eprintln!(
                    "{} fps captured, {} fps shown",
                    seq - last_report_seq,
                    blits - last_report_blits
                );
            } else {
                eprintln!("{} fps", seq - last_report_seq);
            }
            last_report_seq = seq;
            last_report_blits = blits;
            last_report = Instant::now();
        }

        // Short timeout so the window stays responsive. xfer_cb runs on this
        // thread, inside this call, which is why FrameState needs no lock.
        let tv = timeval { tv_sec: 0, tv_usec: 5_000 };
        unsafe {
            ffi::libusb_handle_events_timeout(ctx.as_raw(), &tv);
        }

        if window.is_key_pressed(Key::S, KeyRepeat::No) {
            unsafe { (*state).save_ppm() };
        }

        // Mode 4 is a render-side toggle rather than a capture mode: it
        // composes with whichever of 0-3 the camera is currently in, so it
        // needs no restart and no register write.
        if window.is_key_pressed(Key::Key4, KeyRepeat::No) {
            interp_on = !interp_on;
            println!("frame interpolation {}", if interp_on { "on" } else { "off" });
        }

        // Mode switch. The kernel driver changes format the same way: stop
        // streaming, then start again with the new mode. init() stays a
        // one-shot, exactly as sd_init_72a is only called at probe.
        for (i, key) in [Key::Key0, Key::Key1, Key::Key2, Key::Key3]
            .into_iter()
            .enumerate()
        {
            if !window.is_key_pressed(key, KeyRepeat::No) || MODES[i] == mode {
                continue;
            }
            mode = MODES[i];
            println!("switching to mode {i}: {}x{}", mode.w, mode.h);
            unsafe {
                // Stop the camera first so the cancelled transfers come back
                // promptly instead of racing incoming data.
                cam.stop();
                tear_down_ring(&ctx, state, ring)?;
                (*state).set_mode(mode);
                cam.start(mode);
                ring = build_ring(cam.handle.as_raw(), ep_addr, best, state)?;
            }
            last_report_seq = unsafe { (*state).seq };
        }

        // Blit at a fixed ~60 Hz whether or not a new frame arrived: at this
        // resolution it costs nothing, and it keeps the window responsive
        // between frames without a separate message pump.
        if last_pump.elapsed() >= IDLE_PUMP {
            last_pump = Instant::now();
            // Borrow only for the blit, never across the FFI call above.
            let s = unsafe { &*state };
            let (w, h) = (s.mode.w, s.mode.h);
            if interp_on && s.have_prev {
                // The pair is prepared once however many phases are drawn from
                // it, so only compose() below runs per blit.
                if prepared_for != s.seq {
                    engine.prepare(&s.prev_rgb, &s.rgb, w, h);
                    prepared_for = s.seq;
                }
                // Phase on the wall clock: prev is shown as cur lands, and cur
                // is reached one interval later. That trailing interval is the
                // one frame of latency interpolation cannot avoid -- it needs
                // both ends of the gap before it can fill it.
                let t = (s.frame_at.elapsed().as_secs_f64() / s.frame_dt).clamp(0.0, 1.0);
                let frame = engine.compose(&s.prev_rgb, &s.rgb, w, h, t as f32);
                window.update_with_buffer(frame, w, h)?;
            } else {
                window.update_with_buffer(&s.rgb, w, h)?;
            }
            blits += 1;
        }
    }

    cam.stop();
    if let Err(e) = unsafe { tear_down_ring(&ctx, state, ring) } {
        eprintln!("{e}");
    }
    println!("saved {} frames", unsafe { (*state).count });
    Ok(())
}

/// Minimal Ctrl-C handling without pulling in the ctrlc crate.
/// Swap for the `ctrlc` crate if you'd rather.
fn ctrlc_shim<F: Fn() + Send + 'static>(f: F) {
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        f();
    });
    println!("(press Enter to stop)");
}
