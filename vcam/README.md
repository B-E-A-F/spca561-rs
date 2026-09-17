# Virtual camera

Publishes the capture as a real Windows camera, so it shows up in
**Settings > Bluetooth & devices > Cameras** and to any application, rather
than needing a preview window screen-scraped.

Media Foundation rather than DirectShow, deliberately. MF registers through the
Frame Server -- the same path a physical camera takes -- which is what the
Settings page and the Camera app read, and the stream is consumable by
DirectShow applications too. A DirectShow filter is the narrower choice: it is
why OBS's virtual camera works in Zoom but never appears in Settings.

## Requirements

- Windows 11 build 22000 or newer (`MFCreateVirtualCamera`)
- Visual Studio Build Tools with the Desktop C++ workload
- **Administrator, once, to register.** See below -- this is not optional.

## Build

```
vcam\build.cmd
```

Produces `build\vcam_source.dll` (the COM media source) and
`build\vcam_host.exe`.

## Use

Register once, from an **elevated** prompt:

```
vcam\build\vcam_host.exe register
```

`0x00000000` means machine-wide. `0x00000001` means it fell back to per-user,
which will not work -- see [Why elevation](#why-elevation).

Then, in three windows:

```
vcam\build\vcam_host.exe run          camera exists while this runs
set SPCA_VCAM=1 && cargo run --release    capture, publishing frames
vcam\build\vcam_host.exe list         should show it alongside real cameras
```

| Command | |
|---------|--|
| `register` / `unregister` | COM registration |
| `run` | create the camera and hold it open |
| `list` | enumerate cameras the way an application does |
| `peek` | read the shared mapping directly, without the camera |
| `grab` | open the camera and read frames, end to end |

`peek` and `grab` exist to split failures apart. A black picture can mean the
capture is not publishing, or that the media source is not delivering; `peek`
answers the first without involving the camera at all, and `grab` covers the
whole path.

## How it fits together

The media source is an **in-process COM server**, so Windows loads it inside
whichever application opens the camera -- Teams, the Camera app, a browser. It
cannot be part of the Rust program and cannot call into it. They meet through a
block of shared memory, described in `src/shared.h`, written by the Rust side
and read by the source. Keep the header layout in step: it is duplicated in
`main.rs`, and `peek` prints `sizeof(VcamHeader)` so the two can be checked
against each other.

Frames are published at a fixed 352x288 BGRX regardless of capture mode,
because an application picks a media type once and keeps it -- switching mode
must not change what the camera claims to be. Only captured frames are
published, not interpolated ones: the consumer has its own clock and re-times
the stream anyway, so invented phases would add latency for nothing.

## Why elevation

`IMFVirtualCamera::Start` hands the camera to the **Frame Server service**,
which runs under its own account and never sees `HKEY_CURRENT_USER`. Registered
per user, the CLSID resolves in our process -- the activator even runs -- and
then `Start` fails with `ERROR_PATH_NOT_FOUND` from a service that cannot find
a class which, from where it is standing, does not exist.

`DllRegisterServer` tries HKLM, falls back to HKCU, and returns `S_FALSE` for
the fallback so it cannot be mistaken for having worked.

## Things that cost time

**Do not force-kill `vcam_host`.** Quit it with Enter. A killed host never
reaches `Remove()`, and the camera registration outlives the process that made
it -- the next `run` then fails with `MF_E_INVALIDREQUEST` for trying to
register a camera that, as far as the frame server is concerned, already
exists. Restarting the service clears it.

**The DLL stays locked after use.** The Frame Server service keeps it loaded
after the camera goes away, so rebuilding fails with `LNK1104: cannot open
file`. Restart the service from an elevated prompt:

```
Restart-Service FrameServer -Force
```

**A missing interface says nothing about itself.** The frame server queries the
object for the interfaces a camera driver would expose, and anything absent
surfaces as a bare `E_NOINTERFACE` from `Start` naming nothing. The DLL logs
refused IIDs to `build\vcam_qi.log`, which is the only practical way to find
out what it wanted. What it wanted, in the end:

- The registered CLSID must produce an **`IMFActivate`**, not the media source.
  The frame server creates the activator in its own process and asks it for the
  source there. Registering the source directly is the obvious mistake.
- The source must also answer `IMFMediaSourceEx`, `IMFGetService` and
  `IKsControl`; the stream, `IMFMediaStream2` and `IKsControl`.
- `IMFCollection` and `IMFRealTimeClientEx` are queried too, but are probes.
  Refusing them is correct.

**The activator must not shut the source down when it is released.** Media
Foundation routinely drops the activator as soon as it holds the source, and
tearing the source down there leaves the frame server with an object that
answers `MF_E_SHUTDOWN` to everything.

**`MENewStream` must be queued exactly once, carrying the stream.** An
announcement with no stream attached is not a weaker version of the event, it
is a malformed one, and the consumer rejects the whole start with
`E_INVALIDARG`.
