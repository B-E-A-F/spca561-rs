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
            draining: false,
            inflight: 0,
        }
    }

    /// Resize for a new mode and drop any half-assembled frame.
    fn set_mode(&mut self, mode: CamMode) {
        self.mode = mode;
        self.buf = vec![0u8; mode.frame_sz()];
        self.rgb = vec![0u32; mode.frame_sz()];
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
        self.seq = self.seq.wrapping_add(1);
    }

    /// Write the frame currently on screen as a binary PPM. Called from the
    /// window loop on a keypress, not from the USB callback.
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
    let mut mode = MODES[0];
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
        "SPCA561A live  -  0-3 mode, S saves a PPM, Esc quits",
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

    let mut last_pump = Instant::now();
    let mut last_report = Instant::now();
    let mut last_report_seq = 0u64;
    while running.load(Ordering::SeqCst)
        && window.is_open()
        && !window.is_key_down(Key::Escape)
    {
        if last_report.elapsed() >= Duration::from_secs(1) {
            let seq = unsafe { (*state).seq };
            eprintln!("{} fps", seq - last_report_seq);
            last_report_seq = seq;
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
            window.update_with_buffer(&s.rgb, s.mode.w, s.mode.h)?;
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
