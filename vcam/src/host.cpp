// Registers the media source and publishes it as a Windows camera.
//
//   vcam_host register     write the COM registration (HKCU, no admin)
//   vcam_host unregister    remove it
//   vcam_host run           create the virtual camera and hold it open
//
// The camera exists only while `run` is running. That is deliberate: with
// MFVirtualCameraLifetime_Session the registration disappears when this exits,
// so a crash or a reboot cannot leave a dead camera advertised to every
// application on the machine. It also means no elevation -- Session lifetime
// and CurrentUser access both stay inside this user's world.
#include "shared.h"

#include <windows.h>
#include <mfapi.h>
#include <mfidl.h>
#include <mfvirtualcamera.h>
#include <mfreadwrite.h>
#include <cstdio>
#include <cwchar>

#pragma comment(lib, "mfplat.lib")
#pragma comment(lib, "mf.lib")
#pragma comment(lib, "mfuuid.lib")
#pragma comment(lib, "mfsensorgroup.lib")
#pragma comment(lib, "mfreadwrite.lib")
#pragma comment(lib, "ole32.lib")

// Must match source.cpp. Written here as a string because that is the form
// MFCreateVirtualCamera wants for sourceId.
static const wchar_t *kClsid = L"{7F3C1A52-9D84-4E6B-B0A7-2C5E8D1F4A93}";
static const wchar_t *kFriendlyName = L"SPCA561A";
static const wchar_t *kDllName = L"vcam_source.dll";

typedef HRESULT(STDAPICALLTYPE *DllEntry)(void);

// Resolve the DLL next to this executable rather than by search order, so
// registration can never point at a different copy than the one intended.
static bool DllPath(wchar_t *out, size_t n) {
    if (!GetModuleFileNameW(nullptr, out, (DWORD)n)) return false;
    wchar_t *slash = wcsrchr(out, L'\\');
    if (!slash) return false;
    slash[1] = 0;
    return wcscat_s(out, n, kDllName) == 0;
}

static int CallDllEntry(const char *which) {
    wchar_t path[MAX_PATH];
    if (!DllPath(path, MAX_PATH)) {
        std::printf("could not work out the DLL path\n");
        return 1;
    }
    HMODULE dll = LoadLibraryW(path);
    if (!dll) {
        std::wprintf(L"could not load %s (error %lu)\n", path, GetLastError());
        return 1;
    }
    auto fn = (DllEntry)GetProcAddress(dll, which);
    if (!fn) {
        std::printf("%s not exported\n", which);
        FreeLibrary(dll);
        return 1;
    }
    HRESULT hr = fn();
    std::printf("%s: 0x%08lx\n", which, (unsigned long)hr);
    if (hr == S_FALSE) {
        std::printf("\nRegistered for this user only -- HKLM was refused.\n"
                    "The Frame Server service runs under another account and\n"
                    "cannot see HKCU, so the camera will fail to start with\n"
                    "ERROR_PATH_NOT_FOUND. Re-run this from an elevated\n"
                    "prompt to register machine-wide.\n");
    }
    FreeLibrary(dll);
    return SUCCEEDED(hr) ? 0 : 1;
}

static int Run() {
    HRESULT hr = MFStartup(MF_VERSION, MFSTARTUP_FULL);
    if (FAILED(hr)) {
        std::printf("MFStartup failed: 0x%08lx\n", (unsigned long)hr);
        return 1;
    }

    BOOL supported = FALSE;
    hr = MFIsVirtualCameraTypeSupported(MFVirtualCameraType_SoftwareCameraSource,
                                        &supported);
    if (FAILED(hr) || !supported) {
        std::printf("software virtual cameras are not supported here "
                    "(hr 0x%08lx, supported %d). Needs Windows 11 build "
                    "22000 or newer.\n",
                    (unsigned long)hr, (int)supported);
        MFShutdown();
        return 1;
    }

    IMFVirtualCamera *cam = nullptr;
    hr = MFCreateVirtualCamera(MFVirtualCameraType_SoftwareCameraSource,
                               MFVirtualCameraLifetime_Session,
                               MFVirtualCameraAccess_CurrentUser,
                               kFriendlyName, kClsid, nullptr, 0, &cam);
    if (FAILED(hr)) {
        std::printf("MFCreateVirtualCamera failed: 0x%08lx\n",
                    (unsigned long)hr);
        std::printf("if this is 0x80070002, the source is not registered -- "
                    "run 'vcam_host register' first.\n");
        MFShutdown();
        return 1;
    }

    hr = cam->Start(nullptr);
    if (FAILED(hr)) {
        std::printf("IMFVirtualCamera::Start failed: 0x%08lx\n",
                    (unsigned long)hr);
        if (hr == MF_E_INVALIDREQUEST) {
            std::printf(
                "\nMF_E_INVALIDREQUEST usually means a camera for this source\n"
                "is already registered -- normally because a previous run was\n"
                "force-killed, so it never got to Remove() its camera. The\n"
                "registration outlives the process that made it.\n\n"
                "Clear it by restarting the frame server, elevated:\n"
                "    Restart-Service FrameServer -Force\n\n"
                "And quit this with Enter rather than killing it, so the\n"
                "camera is removed on the way out.\n");
        }
        cam->Remove();
        cam->Release();
        MFShutdown();
        return 1;
    }

    std::wprintf(L"Virtual camera \"%s\" is live.\n", kFriendlyName);
    std::printf("Windows appends \"Windows Virtual Camera\" to the name in its "
                "own UI.\nIt is listed under Settings > Bluetooth & devices > "
                "Cameras while this runs.\n\nPress Enter to remove it.\n");
    (void)getchar();

    cam->Stop();
    cam->Remove();
    cam->Release();
    MFShutdown();
    std::printf("removed.\n");
    return 0;
}

// Enumerate video capture devices the way an application does, so "is it
// there" is answered by the same mechanism Teams or the Camera app uses rather
// than by looking at a settings page.
static int List() {
    HRESULT hr = MFStartup(MF_VERSION, MFSTARTUP_FULL);
    if (FAILED(hr)) {
        std::printf("MFStartup failed: 0x%08lx\n", (unsigned long)hr);
        return 1;
    }

    IMFAttributes *attrs = nullptr;
    hr = MFCreateAttributes(&attrs, 1);
    if (SUCCEEDED(hr)) {
        hr = attrs->SetGUID(MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                            MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID);
    }

    IMFActivate **devices = nullptr;
    UINT32 count = 0;
    if (SUCCEEDED(hr)) hr = MFEnumDeviceSources(attrs, &devices, &count);
    if (FAILED(hr)) {
        std::printf("MFEnumDeviceSources failed: 0x%08lx\n", (unsigned long)hr);
        if (attrs) attrs->Release();
        MFShutdown();
        return 1;
    }

    std::printf("%u video capture device(s):\n", count);
    for (UINT32 i = 0; i < count; ++i) {
        wchar_t *name = nullptr;
        UINT32 len = 0;
        if (SUCCEEDED(devices[i]->GetAllocatedString(
                MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &name, &len))) {
            std::wprintf(L"  [%u] %s\n", i, name);
            CoTaskMemFree(name);
        } else {
            std::printf("  [%u] (no friendly name)\n", i);
        }
        devices[i]->Release();
    }
    CoTaskMemFree(devices);
    if (attrs) attrs->Release();
    MFShutdown();
    return 0;
}

// Read the shared mapping directly, without involving the camera at all.
// Splits "is the publisher working" from "is the camera working", which are
// otherwise one indivisible black screen.
static int Peek() {
    // The header layout is duplicated in Rust, so prove the two agree before
    // trusting anything read through it.
    std::printf("sizeof(VcamHeader) = %zu (Rust assumes 40)\n",
                sizeof(VcamHeader));

    vcam::FrameReader reader;
    if (!reader.open()) {
        std::printf("no mapping -- run the capture program with SPCA_VCAM=1\n");
        return 1;
    }

    std::vector<uint8_t> frame;
    uint32_t w = 0, h = 0;
    uint64_t qpc = 0, seq = 0;
    for (int i = 0; i < 5; ++i) {
        if (!reader.read(frame, w, h, qpc, seq)) {
            std::printf("[%d] could not read a clean frame\n", i);
            Sleep(200);
            continue;
        }
        // Mean luma, to tell a real picture from a black one.
        uint64_t sum = 0;
        size_t n = 0;
        for (size_t p = 0; p + 3 < frame.size(); p += 4 * 17) {
            sum += (uint64_t)(frame[p + 2] * 77 + frame[p + 1] * 150 +
                              frame[p] * 29) >> 8;
            ++n;
        }
        std::printf("[%d] %ux%u seq %llu mean luma %llu\n", i, w, h,
                    (unsigned long long)seq,
                    (unsigned long long)(n ? sum / n : 0));
        Sleep(300);
    }
    return 0;
}

// Open the virtual camera and read frames from it, exactly as an application
// would. This is the only test that covers the whole path: capture, shared
// memory, the media source, the frame server, and back out to a consumer.
static int Grab() {
    HRESULT hr = MFStartup(MF_VERSION, MFSTARTUP_FULL);
    if (FAILED(hr)) return 1;

    IMFAttributes *attrs = nullptr;
    hr = MFCreateAttributes(&attrs, 1);
    if (SUCCEEDED(hr)) {
        hr = attrs->SetGUID(MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                            MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID);
    }

    IMFActivate **devices = nullptr;
    UINT32 count = 0;
    if (SUCCEEDED(hr)) hr = MFEnumDeviceSources(attrs, &devices, &count);
    if (FAILED(hr)) {
        std::printf("enumeration failed: 0x%08lx\n", (unsigned long)hr);
        MFShutdown();
        return 1;
    }

    IMFMediaSource *source = nullptr;
    for (UINT32 i = 0; i < count; ++i) {
        wchar_t *name = nullptr;
        UINT32 len = 0;
        if (SUCCEEDED(devices[i]->GetAllocatedString(
                MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &name, &len))) {
            if (wcsstr(name, kFriendlyName) && !source) {
                std::wprintf(L"opening %s\n", name);
                devices[i]->ActivateObject(IID_PPV_ARGS(&source));
            }
            CoTaskMemFree(name);
        }
        devices[i]->Release();
    }
    CoTaskMemFree(devices);
    attrs->Release();

    if (!source) {
        std::printf("virtual camera not found -- is 'vcam_host run' going?\n");
        MFShutdown();
        return 1;
    }

    IMFSourceReader *reader = nullptr;
    hr = MFCreateSourceReaderFromMediaSource(source, nullptr, &reader);
    if (FAILED(hr)) {
        std::printf("MFCreateSourceReaderFromMediaSource failed: 0x%08lx\n",
                    (unsigned long)hr);
        source->Release();
        MFShutdown();
        return 1;
    }

    for (int i = 0; i < 8; ++i) {
        DWORD streamIndex = 0, flags = 0;
        LONGLONG ts = 0;
        IMFSample *sample = nullptr;
        hr = reader->ReadSample((DWORD)MF_SOURCE_READER_FIRST_VIDEO_STREAM, 0,
                                &streamIndex, &flags, &ts, &sample);
        if (FAILED(hr)) {
            std::printf("[%d] ReadSample failed: 0x%08lx\n", i,
                        (unsigned long)hr);
            break;
        }
        if (!sample) {
            std::printf("[%d] no sample (flags 0x%lx)\n", i, flags);
            continue;
        }

        IMFMediaBuffer *buf = nullptr;
        if (SUCCEEDED(sample->ConvertToContiguousBuffer(&buf))) {
            BYTE *p = nullptr;
            DWORD len = 0;
            if (SUCCEEDED(buf->Lock(&p, nullptr, &len))) {
                uint64_t sum = 0;
                size_t n = 0;
                for (DWORD q = 0; q + 3 < len; q += 4 * 17) {
                    sum += (uint64_t)(p[q + 2] * 77 + p[q + 1] * 150 +
                                      p[q] * 29) >> 8;
                    ++n;
                }
                std::printf("[%d] %lu bytes, ts %lld, mean luma %llu\n", i, len,
                            (long long)ts, (unsigned long long)(n ? sum / n : 0));
                buf->Unlock();
            }
            buf->Release();
        }
        sample->Release();
    }

    reader->Release();
    source->Shutdown();
    source->Release();
    MFShutdown();
    return 0;
}

int main(int argc, char **argv) {
    const char *cmd = argc > 1 ? argv[1] : "run";
    if (strcmp(cmd, "register") == 0) return CallDllEntry("DllRegisterServer");
    if (strcmp(cmd, "unregister") == 0)
        return CallDllEntry("DllUnregisterServer");
    if (strcmp(cmd, "run") == 0) return Run();
    if (strcmp(cmd, "list") == 0) return List();
    if (strcmp(cmd, "peek") == 0) return Peek();
    if (strcmp(cmd, "grab") == 0) return Grab();

    std::printf("usage: vcam_host [register|unregister|run|list|peek|grab]\n");
    return 2;
}
