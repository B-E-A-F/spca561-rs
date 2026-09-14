# spca561

Userspace capture for the Sunplus **SPCA561A** (Rev072A) USB webcam, USB ID
`04fc:0561`, with a live preview window.

Target: Windows + a userspace USB driver (WinUSB or libusbK, bound with
[Zadig]) via `rusb`/`libusb`. Nothing here is Windows-specific by design, but
that is what it has been run on.

[Zadig]: https://zadig.akeo.ie/

## Setup

### 1. Prerequisites

- A Rust toolchain ([rustup]). On Windows use the MSVC toolchain, which needs
  the Visual Studio Build Tools with the "Desktop development with C++"
  workload.
- No MSYS2 or system libusb is needed: `rusb` vendors and builds libusb itself.

[rustup]: https://rustup.rs/

### 2. Bind the camera with Zadig

Windows binds this camera to its own USB video class driver, which does not
expose the raw isochronous endpoint this program needs. Zadig swaps that for a
userspace driver -- either **WinUSB** or **libusbK**. Both work with `rusb`;
which one you want depends on the machine, so read [Which driver](#which-driver)
below before choosing.

> **Read this first.** Replacing the driver means the camera stops working as a
> normal webcam in Teams, OBS, Zoom and so on until you revert it. See
> [Reverting](#reverting) below. Only do this to a camera you are willing to
> take out of normal service.

1. Plug the camera in.
2. Download and run [Zadig]. It is portable -- no install. **Run it as
   Administrator**, or driver replacement silently fails.
3. Tick **Options -> List All Devices**. Without this the camera will not appear.
4. Pick the camera in the dropdown. **Confirm the USB ID reads `04FC 0561`** in
   the fields under the dropdown -- do not go by name alone. Replacing the
   driver on the wrong device (a mouse, a keyboard, a hub) will disable it.
5. Set the target driver on the right of the green arrow to **WinUSB**, or to
   **libusbK** -- see below.
6. Click **Replace Driver**, and wait. It can sit there for 30 seconds or so.
7. When it reports success, unplug the camera and plug it back in.

If the device has several interfaces listed, choose the one carrying the
isochronous video endpoint. The program prints which endpoint and alternate
setting it selected at startup, so it will tell you if you picked wrong.

#### Which driver

**WinUSB** is Microsoft's in-box driver and the obvious first choice. Its
isochronous support, though, is a Windows 8.1-era API that does not work
everywhere -- and when it fails it fails completely, rejecting every transfer
the instant it is submitted while control transfers carry on working perfectly.

**libusbK** ships its own driver with an older, more permissive isochronous
path. If WinUSB gives you no data, rebind to libusbK. Nothing in this program
changes; `rusb` picks it up transparently.

Rebinding between the two is just step 2 again with a different selection in
the dropdown.

### 3. Run

```
cargo run --release
```

A window opens with the live feed.

**Nothing here needs downloading.** The default build has four dependencies, no
ONNX Runtime and no model files, and every feature described below works: all
four capture modes, autogain, both demosaics, frame interpolation, and
resampling into the window.

The optional `onnx` feature adds two neural engines -- RIFE for interpolation
and Real-ESRGAN for upscaling -- which need model files fetched separately. They
are strictly additions. Build with the feature but without the models and the
program still runs: it falls back to the built-in engines, says so only if you
asked for a model it could not find, and never fails to start over one.

| Key | Action |
|-----|--------|
| `0`-`3` | Switch capture mode (see [Modes](#modes)) |
| `4` | Toggle frame interpolation (see [Mode 4](#mode-4-frame-interpolation)) |
| `5` | Switch demosaic (see [Demosaic](#demosaic)) |
| `6` | Toggle upscaling (see [Upscaling](#upscaling)) |
| `A` | Toggle autogain (see [Exposure](#exposure)) |
| `S` | Write the current frame to `frame_NNNN.ppm` |
| `Esc` | Quit |

The window is sized for the largest mode and stays that size; smaller modes are
scaled up into it. With an upscaler loaded it is sized for the largest mode at
the model's scale factor instead, since minifb cannot resize a window after it
is created.

At startup it prints the endpoint it chose, then a frame rate once a second:

```
using alt 7, ep 0x81, 1023 bytes/packet
streaming, Esc or close the window to stop
5 fps
5 fps
```

The alternate setting number varies between devices -- it picks the isochronous
IN endpoint with the largest packet size, whichever alt that is.

Environment variables set the startup state, which saves clicking the window
before pressing a key, and makes the thing scriptable:

| Variable | Effect |
|----------|--------|
| `SPCA_MODE` | Initial capture mode, `0`-`3` |
| `SPCA_INTERP` | `1` to start with interpolation already on |
| `SPCA_AUTOGAIN` | `0` to start with autogain off |
| `SPCA_AG_MAX_EXPO` | Autogain exposure ceiling in hex, default `256` |
| `SPCA_DEMOSAIC` | `bilinear` (default) or `block` |
| `SPCA_SHOT` | Capture this many frames to PPM, then exit |
| `SPCA_SHOT_AB` | `1` to write each shot through both demosaics |
| `SPCA_RIFE_MODEL` | Path to the RIFE model, when built with `--features rife` |
| `SPCA_SR_MODEL` | Path to a Real-ESRGAN model; loads the upscaler |
| `SPCA_SR` | `0` to load the upscaler but start with it off |
| `SPCA_DIAG` | `1` to report lost frames, stage timings and pipeline latency |

If the frame rate sits at `0 fps` while the program is otherwise running, see
[Troubleshooting](#troubleshooting).

## Troubleshooting

**`device 04fc:0561 not found. Did you bind WinUSB with Zadig?`**
The camera is not bound to WinUSB or libusbK. Redo step 2, making sure
**List All Devices** is ticked and Zadig is elevated.

**`isochronous submit failed` / `LIBUSB_ERROR_NOT_SUPPORTED` (-12)**
WinUSB isochronous transfer needs Windows 8.1 or newer.

**`timed out reclaiming N isochronous transfers`**
A mode switch could not get its transfers back from libusb within two seconds,
so the ring was leaked rather than freed underneath it and the program stopped.
This should not happen; please report it with the mode you switched from and to.

**`0 fps`, or a frozen preview with no error at all.**
Two unrelated faults produce exactly this, and nothing in the normal output
separates them: in one, no packet ever arrives; in the other, packets arrive but
never delimit into a frame. Work through them in this order.

1. **The driver's isochronous path.** Rebind with Zadig and choose **libusbK**
   instead of WinUSB, then re-run. If that fixes it, WinUSB had been rejecting
   every transfer the moment it was submitted.

   libusb carries two separate isochronous implementations and picks between
   them by bound driver -- the `SUB_API_LIBUSBK` / `SUB_API_WINUSB` branch in
   the submit path of `windows_winusb.c`. libusbK submits a single
   `IsoReadPipe()` with an explicit start frame. WinUSB instead pairs
   `RegisterIsochBuffer()` with `ReadIsochPipeAsap()`, which schedules "as soon
   as possible" and has to establish a continuing stream; where it cannot, it
   fails the request outright rather than degrading. Control transfers are
   untouched by any of this, which is what makes the symptom so misleading:
   every register write succeeds, the device sits healthy in Device Manager, and
   the program looks like it is running normally.

   Seen on an Intel xHCI root port with the camera attached directly: WinUSB
   rejected every transfer at every alternate setting, from 128 up to 1023 bytes
   per packet, while libusbK worked immediately on the same port.

2. **Frame geometry.** If libusbK changes nothing, packets are most likely
   arriving and failing to delimit, which is the signature of `init()` not
   having run. See [the note below](#the-bridge-needs-initialising-before-it-will-delimit-frames).

## Reverting

To give the camera back to Windows: Device Manager -> find the device ->
**Uninstall device**, tick *Delete the driver software for this device*, then
unplug and replug. Windows reinstalls its own driver. This is the same whether
you bound WinUSB or libusbK.

## Notes

The camera is USB 1.1 full speed, so isochronous transfer caps at 1023 bytes per
1 ms frame. At 352x288 a raw Bayer frame is 101,376 bytes -- about 99 packets,
or roughly 5-10 fps. That is a bandwidth ceiling, not a software limit; drop to
a smaller mode for a faster feed.

### Modes

Press `0`-`3` while running to switch. All Rev072A modes are raw SGBRG8 Bayer,
uncompressed:

| Key | Resolution | Approx. fps |
|-----|------------|-------------|
| `0` | 352x288    | ~5-10       |
| `1` | 320x240    | ~7-13       |
| `2` | 176x144    | ~20-40      |
| `3` | 160x120    | ~25-50      |

The link is bandwidth-limited rather than sensor-limited, so a quarter of the
pixels buys roughly four times the frame rate.

Switching follows the kernel driver's own format-change path: stop streaming
(`sd_stopN`), then start again with the new mode (`sd_start_72a`). `init()` is
not repeated, matching `sd_init_72a` being called only at probe. The
isochronous ring is cancelled and fully drained before being rebuilt -- freeing
a transfer while libusb still has a callback pending is undefined behaviour, so
teardown waits for every transfer to come back, and on timeout leaks the ring
rather than freeing memory libusb may still write to.

### Upscaling

`6` toggles Real-ESRGAN super-resolution, behind the same `onnx` feature as
RIFE. Point `SPCA_SR_MODEL` at a model and it loads at startup:

```
SPCA_SR_MODEL=esrgan/RealESRGANv2-animevideo-xsx4.onnx cargo run --release --features rife
```

The models come from vs-mlrt's `model-20211209` release, `RealESRGANv2_v1.7z`
(4.3 MB) and `RealESRGANv3_v1.7z` (2.3 MB). The contract is far simpler than
RIFE's: one `1x3xHxW` float tensor of RGB in 0..1 in, the same shape out at the
model's scale, every dimension dynamic, so no padding or alignment is needed.

The scale factor is measured at startup by pushing a 32x32 probe through and
seeing how big it comes back, rather than trusted from the filename. It has to
be known before the window is created and minifb cannot resize afterwards --
which is also why the upscaler loads before the window, and why `6` toggles
only whether it is applied, never the window size.

Upscaling runs last, on whatever was about to be shown, so it composes with
interpolation without either knowing about the other. That ordering is the
cheap one: the interpolator gets to work on small frames, where the reverse
would have it chewing through megapixel frames instead.

Measured on this camera with the 4x model on DirectML:

| Mode | Output | Displayed |
|------|--------|-----------|
| 3 (160x120) | 640x480 | ~44-50 fps |
| 0 (352x288) | 1408x1152 | ~25 fps |

**It costs latency, not throughput.** The forward pass takes about 15 ms per
displayed frame at 160x120 and does not get cheaper with a smaller model: the
2x, 4x and v3 models all measure within a few percent of each other, because at
these sizes the cost is per-layer dispatch overhead rather than pixels. Shown
frame rate barely moves, but the render pipeline goes from 3.4 ms to about 20
ms per frame, measurable with `SPCA_DIAG=1`.

That matters for interpolation. The phase is chosen before that work runs and
the result is only seen after it, so without correction every displayed frame
trails reality by the pipeline's own latency -- which reads as lag, and on a
moving subject as a doubled blend that looks like blur. The phase is therefore
advanced by a smoothed measurement of that latency, and events are reaped
immediately before composing so the frame being shown is the freshest captured
one rather than up to a render interval old. Neither mattered at 3 ms; both do
at 20.

**What it does and does not do.** These are the `animevideo` variants, anime
trained, which is what vs-mlrt ships. On structural edges they are genuinely
good: a diagonal stair rail in a test frame goes from jagged and noisy to a
clean continuous line. On texture they flatten, turning hair into smooth
gradients rather than resolving strands. That suits this sensor, which has few
fine textures to lose and plenty of edges to clean up, but it is denoising and
smoothing more than it is recovering detail. Nothing here puts back information
that 352x288 never captured.

Do the exposure work before reaching for this. Fed an underexposed frame, a
super-resolution pass magnifies noise into confident invented texture.

`SPCA_SHOT` also writes `frame_NNNN_sr.ppm` while upscaling is on, taken from
the captured frame rather than an interpolated phase.

#### With upscaling off

The window keeps the size it was created at, so the frame still has to reach
it. Left to minifb that is `ScaleMode::Stretch`, which blows the frame up with
no filtering at all -- and at 4x the result is blocky enough that interpolation
artefacts the network had been smoothing over become obvious. It reads as
interpolation getting worse when nothing about it has changed; interpolation
works on native frames and never sees the window.

So with the model off the frame is resampled to the window instead, separable
bilinear with tap positions and weights precomputed per size. Measured in mode
0, that fills 1408x1152 at ~55 fps against the model's ~23, so the honest path
costs nothing and doubles as the A/B: `6` now compares plain resampling with
the network at the same output size, rather than comparing a stretch with it.

The same applies without any upscaler, where the window is sized for mode 0 and
the smaller modes are resampled into it rather than stretched.

### Exposure

The sensor powers up badly underexposed indoors and nothing in `init()` or
`start()` corrects it -- the kernel driver leans entirely on its autogain loop
to find a working exposure, so this does too. It is on by default. `A` toggles
it, `SPCA_AUTOGAIN=0` starts with it off.

The bridge accumulates per-channel luminance in `0x8621`-`0x8624`. Each pass
weights those into a luma, and if it is more than 20 away from a target of 110,
nudges the sensor's exposure (i2c `0x09`) and gain (i2c `0x35`) towards it.
That is `do_autogain()` from `spca561.c`, and the constants are the kernel's.

Measured on this camera, mode 0: mean frame luma goes from about 21 to about
78 and settles there in roughly ten seconds. The metering is whole-scene, so a
bright window in shot will blow out -- the same trade the kernel driver makes.

Two deliberate departures from the kernel:

- **Passes are paced every 5 captured frames, not 13.** The kernel runs per
  frame against a driver doing 25-30 fps; at 5 fps in mode 0 its pacing is a
  pass every 2.6 seconds. Each pass costs a handful of control transfers that
  share the link with the isochronous stream, so 5 is a compromise rather than
  "every frame".
- **The damping loosens while far from target.** The kernel's fixed shift of 4
  is tuned for gentle adaptation from an already-reasonable exposure. From this
  sensor's power-up state it steps by 5 against a range running to 0x256, which
  takes minutes to arrive. Halving the damping outside twice the tolerance
  converges in seconds, and it is restored near the target so the loop settles
  rather than hunting.

Autogain runs from the window loop rather than the USB callback: it makes
blocking control transfers, and stalling `xfer_cb` would starve the ring. For
the same reason the i2c handshakes pump libusb events while waiting instead of
sleeping -- a plain sleep leaves completed transfers unreaped, and the frame
rate visibly drops every time autogain acts.

#### Exposure costs frame rate

They are the same knob on this sensor: longer integration is bought by slowing
the frame clock. Measured in mode 3:

| Exposure ceiling | Capture rate |
|------------------|--------------|
| autogain off     | ~20 fps, dark |
| `0x150`          | ~23 fps |
| `0x256` (kernel default) | 27 fps falling to ~14 as it brightens |

So a well-exposed picture costs roughly half the frame rate, and that is the
sensor rather than any overhead in this program. `SPCA_AG_MAX_EXPO` sets the
ceiling in hex if you would rather keep the frames coming and accept a darker
picture -- worth doing in mode 3, where the point was speed.

A refinement not implemented: gain is free in frame-rate terms where exposure
is not, so preferring gain until it saturates and only then reaching for
exposure would hold the frame rate longer, at the cost of noise. The kernel
raises both together and so does this.

### Demosaic

The sensor is SGBRG8, one measured channel per pixel:

```
even row:  G B G B
odd  row:  R G R G
```

so two channels in three have to be reconstructed. `5` switches between the
two reconstructions at runtime.

**Bilinear** is the default. Each pixel keeps its own measurement and
interpolates the two it lacks from the neighbours that carry them.

**2x2 block** samples one R, G and B per 2x2 block and shares them across all
four pixels. It is what this program originally did, kept because it is the
cheapest thing that works and because every capture before it existed used it.

It is worth understanding what the block version costs, because it is more than
it looks. Sharing one measurement across four pixels means a 352x288 frame
carries only 176x144 distinct colour samples -- half the spatial detail the
sensor actually recorded is discarded before anything else happens. High
contrast edges pick up visible stair-stepping and red/green fringing as a
direct result.

To compare them honestly, capture both from one raw frame:

```
SPCA_SHOT=1 SPCA_SHOT_AB=1 cargo run --release
```

That writes `frame_NNNN_block.ppm` and `frame_NNNN_bilinear.ppm` from the same
sensor data. Switching demosaic between two live captures compares two moments
of a moving scene rather than two algorithms.

### Mode 4: frame interpolation

`4` fills the gaps between captured frames, so the window updates at its own
rate instead of holding each frame until the next one lands. It is not a
capture mode: the camera keeps running in whichever of 0-3 is selected, so `4`
composes with them and needs no restart and no register write.

Pair it with mode 3. At 160x120 the camera already delivers around 20 fps, so
reaching the render loop's ~60 Hz ceiling is barely more than a doubling. Mode
0 at 5 fps needs ten invented frames for every real one, which no interpolator
makes look like motion -- worth trying once, to see where the technique breaks.

Interpolation costs one frame of latency, unavoidably: filling a gap needs both
ends of it, so the newest captured frame is only reached an interval after it
arrives. `S` still saves the most recent real frame, never a composed one.

With it on, the frame rate line reports both figures:

```
20 fps captured, 55 fps shown
```

Two engines exist behind that key.

**Block matching** is built in, needs nothing, and is the default. 8x8 blocks,
a +/-6 pixel search, vectors sampled bilinearly between block centres so the
warp does not show block edges, and a match-quality floor below which a block
falls back to a plain blend -- this sensor is noisy enough that a confidently
wrong vector looks far worse than no vector. Pure integer CPU work; it would
run on a Pi.

**RIFE** is a neural interpolator, behind the `rife` feature:

```
cargo run --release --features rife
```

It needs a RIFE ONNX model, `rife/rife_v4.6.onnx` by default and overridable
with `SPCA_RIFE_MODEL`. The vs-mlrt project publishes exports; `rife_v8.7z`
from their model releases carries v4.0 through v4.10, all sharing one input
contract. A missing model is reported and falls back to block matching, so the
feature can never leave mode 4 broken.

Note that the two builds produce the same binary name, so whichever you built
last is what runs.

The feature is off by default because it pulls ONNX Runtime, a large native
dependency. It asks for DirectML and falls back to CPU if refused; the startup
line names the engine and backend actually in use. At these frame sizes CPU
keeps up with the render loop unaided, so DirectML is a convenience rather than
a requirement -- both hit the same ceiling.

The input tensor is undocumented on the model itself; `src/bin/rife_probe.rs`
prints the contract that `mod rife` is written against, and is the tool to
reach for if a different export disagrees.

### The bridge needs initialising before it will delimit frames

`init()` mirrors the kernel's `sd_init_72a` and **must** run before `start()`
(`sd_start_72a`). The `REV72A_INIT_DATA2` vector it writes carries the
valid-pixel window -- `0x865d` = 0xb0 x2 = 352 wide, `0x865e` = 0x90 x2 = 288
high -- plus the memory buffer threshold and image type.

Without it the bridge has no frame geometry. It still streams pixel data at the
full packet rate, but never emits a start-of-frame marker: `data[0]` free-runs
`0x01`..`0xfe` instead of resetting to `0x00` once per frame, so no frame can
ever be assembled. The symptom is a frozen preview with USB traffic flowing
normally.

## Licence

GPL-2.0-only. The register tables and initialisation ordering are transposed
from the Linux kernel driver `drivers/media/usb/gspca/spca561.c`, which is
GPL-2.0, making this a derivative work. See [LICENSE](LICENSE).
