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
#include <cstdio>
#include <cwchar>

#pragma comment(lib, "mfplat.lib")
#pragma comment(lib, "mf.lib")
#pragma comment(lib, "mfuuid.lib")
#pragma comment(lib, "mfsensorgroup.lib")
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

int main(int argc, char **argv) {
    const char *cmd = argc > 1 ? argv[1] : "run";
    if (strcmp(cmd, "register") == 0) return CallDllEntry("DllRegisterServer");
    if (strcmp(cmd, "unregister") == 0)
        return CallDllEntry("DllUnregisterServer");
    if (strcmp(cmd, "run") == 0) return Run();
    if (strcmp(cmd, "list") == 0) return List();

    std::printf("usage: vcam_host [register|unregister|run|list]\n");
    return 2;
}
