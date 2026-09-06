# spca561### Modes

Press `0`-`3` while running to switch. All Rev072A modes are raw SGBRG8 Bayer,
uncompressed:

| Key | Resolution | Approx. fps |
|-----|------------|-------------|
| `0` | 352x288    | ~5-10 |
| `1` | 320x240    | ~7-13 |
| `2` | 176x144    | ~20-40 |
| `3` | 160x120    | ~25-50 |

Since the link is bandwidth-limited rather than sensor-limited, a quarter of
the pixels buys roughly four times the frame rate.

Switching follows the kernel driver's own format-change path: stop streaming
(`sd_stopN`), then start again with the new mode (`sd_start_72a`). `init()` is
not repeated, matching `sd_init_72a` being called only at probe. The
isochronous ring is cancelled and fully drained before it is rebuilt -- freeing
a transfer while libusb still has a callback pending is undefined behaviour, so
teardown waits for every transfer to come back, and gives up by leaking rather
than freeing early.

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

A window opens with the live feed.

| Key | Action |
|-----|--------|
| `0`-`3` | Switch capture mode (see [Modes](#modes)) |
| `S` | Write the current frame to `frame_NNNN.ppm` |
| `Esc` | Quit |

The window is sized for the largest mode and stays that size; smaller modes are
scaled up into it.

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

**`timed out reclaiming N isochronous transfers`**
A mode switch could not get its transfers back from libusb within two seconds,
so the ring was leaked rather than freed underneath it and the program stopped.
This should not happen; please report it with the mode you switched from and to.

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
