# spca561

Userspace capture for the Sunplus **SPCA561A** (Rev072A) USB webcam, USB ID
`04fc:0561`, with a live preview window.

Target: Windows + WinUSB (bound with [Zadig]) via `rusb`/`libusb`. Nothing here
is Windows-specific by design, but that is what it has been run on.

[Zadig]: https://zadig.akeo.ie/

## Setup

### 1. Prerequisites

- A Rust toolchain ([rustup]). On Windows use the MSVC toolchain, which needs
  the Visual Studio Build Tools with the "Desktop development with C++"
  workload.
- No MSYS2 or system libusb is needed: `rusb` vendors and builds libusb itself.

[rustup]: https://rustup.rs/

### 2. Bind the camera to WinUSB with Zadig

Windows binds this camera to its own USB video class driver, which does not
expose the raw isochronous endpoint this program needs. Zadig swaps that for
WinUSB.

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
5. Set the target driver on the right of the green arrow to **WinUSB**.
6. Click **Replace Driver**, and wait. It can sit there for 30 seconds or so.
7. When it reports success, unplug the camera and plug it back in.

If the device has several interfaces listed, choose the one carrying the
isochronous video endpoint. The program prints which endpoint and alternate
setting it selected at startup, so it will tell you if you picked wrong.

### 3. Run

```
cargo run --release
```

A window opens with the live feed. **Esc** quits, **S** writes the current frame
to `frame_NNNN.ppm` in the working directory.

At startup it prints the endpoint it chose, then a frame rate once a second:

```
using alt 1, ep 0x81, 1023 bytes/packet
streaming, Esc or close the window to stop
5 fps
5 fps
```

If the frame rate sits at `0 fps` while the program is otherwise running, see
the initialisation note below.

## Troubleshooting

**`device 04fc:0561 not found. Did you bind WinUSB with Zadig?`**
The camera is not bound to WinUSB. Redo step 2, making sure **List All Devices**
is ticked and Zadig is elevated.

**`isochronous submit failed` / `LIBUSB_ERROR_NOT_SUPPORTED` (-12)**
WinUSB isochronous transfer needs Windows 8.1 or newer.

**The preview is frozen but there is no error.**
See the note on `init()` below -- that is the exact signature of the bridge
never being told its frame geometry.

## Reverting

To give the camera back to Windows: Device Manager -> find the device ->
**Uninstall device**, tick *Delete the driver software for this device*, then
unplug and replug. Windows reinstalls its own driver.

## Notes

The camera is USB 1.1 full speed, so isochronous transfer caps at 1023 bytes per
1 ms frame. At 352x288 a raw Bayer frame is 101,376 bytes -- about 99 packets,
or roughly 5-10 fps. That is a bandwidth ceiling, not a software limit; drop to
a smaller mode for a faster feed.

`MODE`, `WIDTH` and `HEIGHT` are separate constants and must be changed
together. Rev072A modes are all raw SGBRG8 Bayer, uncompressed:

| MODE | Resolution |
|------|------------|
| 0    | 352x288    |
| 1    | 320x240    |
| 2    | 176x144    |
| 3    | 160x120    |

Debayering is deliberately crude -- nearest-neighbour on 2x2 GBRG blocks -- so
expect colour fringing on edges.

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
