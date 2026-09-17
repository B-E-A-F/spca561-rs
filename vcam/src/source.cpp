// The virtual camera's media source: an in-process COM server that Windows
// loads inside whichever application opens the camera.
//
// Minimal but complete enough for the frame server: a source, one stream, and
// the event plumbing both need. Media Foundation drives this by asking for one
// sample at a time (RequestSample) and expecting an MEMediaSample event back,
// so there is no thread of our own here -- the consumer's cadence is the
// cadence.
//
// It advertises a single fixed format. The camera has four capture modes, but
// an application picks a media type once and keeps it, so switching modes must
// not change what the camera claims to be. The Rust side resamples to this
// size before publishing; anything else that turns up is scaled here rather
// than dropped, because a wrong-sized frame should look wrong, not look like
// the camera is broken.
#include "shared.h"

#include <windows.h>
#include <mfapi.h>
#include <mferror.h>
#include <mfidl.h>
#include <new>
#include <shlwapi.h>
#include <cstdarg>

// The frame server does not settle for a bare IMFMediaSource. It queries the
// object for the interfaces a real camera driver would expose, and a missing
// one comes back as E_NOINTERFACE from IMFVirtualCamera::Start with nothing to
// say which. IKsControl is the property-page plumbing; sources and streams
// both have to answer, even if only to say they support nothing.
#include <ks.h>
#include <ksproxy.h>
#include <ksmedia.h>

#pragma comment(lib, "mfplat.lib")
#pragma comment(lib, "mf.lib")
#pragma comment(lib, "mfuuid.lib")
#pragma comment(lib, "shlwapi.lib")
#pragma comment(lib, "ole32.lib")
#pragma comment(lib, "advapi32.lib")

// {7F3C1A52-9D84-4E6B-B0A7-2C5E8D1F4A93}
// Stable for the life of the project: it is written into the registry and
// named by the host, so changing it orphans any existing registration.
static const GUID CLSID_VCamSource = {
    0x7f3c1a52, 0x9d84, 0x4e6b, {0xb0, 0xa7, 0x2c, 0x5e, 0x8d, 0x1f, 0x4a, 0x93}};

static const UINT32 kWidth = 352;
static const UINT32 kHeight = 288;
static const UINT32 kFpsNum = 30;
static const UINT32 kFpsDen = 1;

static HMODULE g_module = nullptr;
static LONG g_locks = 0;

// Diagnostic only. This DLL is loaded by whoever opens the camera -- possibly
// the frame server service, not us -- so there is no console to print to and
// no debugger attached. A file next to the DLL is the only channel that works
// from every host, and "which interface did it ask for" is otherwise
// unknowable: a missing one surfaces as a bare E_NOINTERFACE from Start with
// nothing naming it.
static void LogLine(const wchar_t *fmt, ...) {
    wchar_t path[MAX_PATH];
    if (!GetModuleFileNameW(g_module, path, MAX_PATH)) return;
    wchar_t *slash = wcsrchr(path, L'\\');
    if (!slash) return;
    slash[1] = 0;
    wcscat_s(path, MAX_PATH, L"vcam_qi.log");

    HANDLE f = CreateFileW(path, FILE_APPEND_DATA, FILE_SHARE_READ | FILE_SHARE_WRITE,
                           nullptr, OPEN_ALWAYS, FILE_ATTRIBUTE_NORMAL, nullptr);
    if (f == INVALID_HANDLE_VALUE) return;

    wchar_t buf[512];
    va_list args;
    va_start(args, fmt);
    int n = vswprintf_s(buf, fmt, args);
    va_end(args);
    if (n > 0) {
        char utf8[1024];
        int m = WideCharToMultiByte(CP_UTF8, 0, buf, n, utf8, sizeof(utf8), nullptr, nullptr);
        DWORD written = 0;
        if (m > 0) WriteFile(f, utf8, (DWORD)m, &written, nullptr);
        WriteFile(f, "\r\n", 2, &written, nullptr);
    }
    CloseHandle(f);
}

static void LogReject(const wchar_t *who, REFIID riid) {
    wchar_t iid[64];
    StringFromGUID2(riid, iid, 64);
    LogLine(L"%s: refused %s", who, iid);
}

// ---------------------------------------------------------------------------

static HRESULT CreateVideoType(IMFMediaType **out) {
    IMFMediaType *type = nullptr;
    HRESULT hr = MFCreateMediaType(&type);
    if (FAILED(hr)) return hr;

    hr = type->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Video);
    if (SUCCEEDED(hr)) hr = type->SetGUID(MF_MT_SUBTYPE, MFVideoFormat_RGB32);
    if (SUCCEEDED(hr))
        hr = type->SetUINT32(MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive);
    if (SUCCEEDED(hr)) hr = type->SetUINT32(MF_MT_ALL_SAMPLES_INDEPENDENT, TRUE);
    if (SUCCEEDED(hr))
        hr = MFSetAttributeSize(type, MF_MT_FRAME_SIZE, kWidth, kHeight);
    if (SUCCEEDED(hr))
        hr = MFSetAttributeRatio(type, MF_MT_FRAME_RATE, kFpsNum, kFpsDen);
    if (SUCCEEDED(hr))
        hr = MFSetAttributeRatio(type, MF_MT_PIXEL_ASPECT_RATIO, 1, 1);
    // Positive stride means top-down. RGB32 in Media Foundation is otherwise
    // bottom-up by convention, which would hand every consumer an upside-down
    // picture -- the classic way this goes wrong.
    if (SUCCEEDED(hr))
        hr = type->SetUINT32(MF_MT_DEFAULT_STRIDE, kWidth * 4);
    if (SUCCEEDED(hr))
        hr = type->SetUINT32(MF_MT_SAMPLE_SIZE, kWidth * kHeight * 4);

    if (FAILED(hr)) {
        type->Release();
        return hr;
    }
    *out = type;
    return S_OK;
}

// Nearest-neighbour, and deliberately so: this is the path for frames that
// should not be arriving at this size at all, so it exists to keep something
// on screen rather than to look good.
static void ScaleBgrx(const uint8_t *src, uint32_t sw, uint32_t sh,
                      uint8_t *dst, uint32_t dw, uint32_t dh) {
    for (uint32_t y = 0; y < dh; ++y) {
        const uint32_t sy = (uint32_t)((uint64_t)y * sh / dh);
        const uint8_t *srow = src + (size_t)sy * sw * 4;
        uint8_t *drow = dst + (size_t)y * dw * 4;
        for (uint32_t x = 0; x < dw; ++x) {
            const uint32_t sx = (uint32_t)((uint64_t)x * sw / dw);
            memcpy(drow + (size_t)x * 4, srow + (size_t)sx * 4, 4);
        }
    }
}

// ---------------------------------------------------------------------------

class VCamSource;

class VCamStream : public IMFMediaStream2, public IKsControl {
public:
    VCamStream(VCamSource *source, IMFStreamDescriptor *sd)
        : source_(source), sd_(sd) {
        sd_->AddRef();
        MFCreateEventQueue(&events_);
        QueryPerformanceFrequency((LARGE_INTEGER *)&qpc_freq_);
    }

    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID riid, void **ppv) override {
        if (!ppv) return E_POINTER;
        if (riid == IID_IUnknown || riid == IID_IMFMediaEventGenerator ||
            riid == IID_IMFMediaStream || riid == IID_IMFMediaStream2) {
            *ppv = static_cast<IMFMediaStream2 *>(this);
        } else if (riid == __uuidof(IKsControl)) {
            *ppv = static_cast<IKsControl *>(this);
        } else {
            LogReject(L"stream", riid);
            *ppv = nullptr;
            return E_NOINTERFACE;
        }
        AddRef();
        return S_OK;
    }

    // IMFMediaStream2 -- the frame server starts and stops individual streams
    // through this rather than only through the source.
    HRESULT STDMETHODCALLTYPE SetStreamState(MF_STREAM_STATE state) override {
        LogLine(L"stream: SetStreamState %d", (int)state);
        switch (state) {
        case MF_STREAM_STATE_RUNNING:
            active_ = true;
            return QueueEvent(MEStreamStarted, GUID_NULL, S_OK, nullptr);
        case MF_STREAM_STATE_STOPPED:
            active_ = false;
            return QueueEvent(MEStreamStopped, GUID_NULL, S_OK, nullptr);
        case MF_STREAM_STATE_PAUSED:
            active_ = false;
            return QueueEvent(MEStreamPaused, GUID_NULL, S_OK, nullptr);
        default:
            return E_INVALIDARG;
        }
    }
    HRESULT STDMETHODCALLTYPE GetStreamState(MF_STREAM_STATE *state) override {
        if (!state) return E_POINTER;
        *state = active_ ? MF_STREAM_STATE_RUNNING : MF_STREAM_STATE_STOPPED;
        return S_OK;
    }

    HRESULT STDMETHODCALLTYPE KsProperty(PKSPROPERTY, ULONG, LPVOID, ULONG,
                                         ULONG *bytes) override {
        if (bytes) *bytes = 0;
        return HRESULT_FROM_WIN32(ERROR_SET_NOT_FOUND);
    }
    HRESULT STDMETHODCALLTYPE KsMethod(PKSMETHOD, ULONG, LPVOID, ULONG,
                                       ULONG *bytes) override {
        if (bytes) *bytes = 0;
        return HRESULT_FROM_WIN32(ERROR_SET_NOT_FOUND);
    }
    HRESULT STDMETHODCALLTYPE KsEvent(PKSEVENT, ULONG, LPVOID, ULONG,
                                      ULONG *bytes) override {
        if (bytes) *bytes = 0;
        return HRESULT_FROM_WIN32(ERROR_SET_NOT_FOUND);
    }
    ULONG STDMETHODCALLTYPE AddRef() override {
        return InterlockedIncrement(&refs_);
    }
    ULONG STDMETHODCALLTYPE Release() override {
        LONG n = InterlockedDecrement(&refs_);
        if (n == 0) delete this;
        return n;
    }

    // IMFMediaEventGenerator -- straight delegation to the queue MF gave us.
    HRESULT STDMETHODCALLTYPE GetEvent(DWORD flags, IMFMediaEvent **ev) override {
        return events_ ? events_->GetEvent(flags, ev) : MF_E_SHUTDOWN;
    }
    HRESULT STDMETHODCALLTYPE BeginGetEvent(IMFAsyncCallback *cb,
                                            IUnknown *state) override {
        return events_ ? events_->BeginGetEvent(cb, state) : MF_E_SHUTDOWN;
    }
    HRESULT STDMETHODCALLTYPE EndGetEvent(IMFAsyncResult *r,
                                          IMFMediaEvent **ev) override {
        return events_ ? events_->EndGetEvent(r, ev) : MF_E_SHUTDOWN;
    }
    HRESULT STDMETHODCALLTYPE QueueEvent(MediaEventType met, REFGUID ext,
                                         HRESULT status,
                                         const PROPVARIANT *v) override {
        return events_ ? events_->QueueEventParamVar(met, ext, status, v)
                       : MF_E_SHUTDOWN;
    }

    HRESULT STDMETHODCALLTYPE GetMediaSource(IMFMediaSource **src) override;

    HRESULT STDMETHODCALLTYPE
    GetStreamDescriptor(IMFStreamDescriptor **sd) override {
        if (!sd) return E_POINTER;
        *sd = sd_;
        sd_->AddRef();
        return S_OK;
    }

    HRESULT STDMETHODCALLTYPE RequestSample(IUnknown *token) override;

    void Shutdown() {
        if (events_) {
            events_->Shutdown();
            events_->Release();
            events_ = nullptr;
        }
        if (sd_) {
            sd_->Release();
            sd_ = nullptr;
        }
    }

    void SetActive(bool active) { active_ = active; }

private:
    ~VCamStream() { Shutdown(); }

    LONG refs_ = 1;
    VCamSource *source_;  // weak: the source owns us and outlives us
    IMFStreamDescriptor *sd_ = nullptr;
    IMFMediaEventQueue *events_ = nullptr;
    bool active_ = false;

    vcam::FrameReader reader_;
    std::vector<uint8_t> frame_;
    std::vector<uint8_t> scaled_;
    uint64_t qpc_freq_ = 0;
    LONGLONG next_pts_ = 0;
};

// ---------------------------------------------------------------------------

class VCamSource : public IMFMediaSourceEx,
                   public IMFGetService,
                   public IKsControl {
public:
    VCamSource() {
        MFCreateEventQueue(&events_);
        InterlockedIncrement(&g_locks);
    }

    HRESULT Init() {
        IMFMediaType *type = nullptr;
        HRESULT hr = CreateVideoType(&type);
        if (FAILED(hr)) return hr;

        // Attribute stores the frame server reads to decide what kind of
        // device this is. Without the frame-source type it is not treated as a
        // colour camera; without the stream category and id the stream is not
        // recognised as a capture pin.
        if (SUCCEEDED(hr)) hr = MFCreateAttributes(&source_attrs_, 2);
        if (SUCCEEDED(hr)) {
            hr = source_attrs_->SetUINT32(MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES,
                                          MFFrameSourceTypes_Color);
        }
        if (SUCCEEDED(hr)) hr = MFCreateAttributes(&stream_attrs_, 3);
        if (SUCCEEDED(hr)) {
            hr = stream_attrs_->SetGUID(MF_DEVICESTREAM_STREAM_CATEGORY,
                                        PINNAME_VIDEO_CAPTURE);
        }
        if (SUCCEEDED(hr)) {
            hr = stream_attrs_->SetUINT32(MF_DEVICESTREAM_STREAM_ID, 0);
        }
        if (SUCCEEDED(hr)) {
            hr = stream_attrs_->SetUINT32(MF_DEVICESTREAM_FRAMESERVER_SHARED, 1);
        }
        if (FAILED(hr)) {
            type->Release();
            return hr;
        }

        IMFStreamDescriptor *sd = nullptr;
        hr = MFCreateStreamDescriptor(0, 1, &type, &sd);
        if (SUCCEEDED(hr)) {
            IMFMediaTypeHandler *handler = nullptr;
            hr = sd->GetMediaTypeHandler(&handler);
            if (SUCCEEDED(hr)) {
                hr = handler->SetCurrentMediaType(type);
                handler->Release();
            }
        }
        if (SUCCEEDED(hr)) {
            IMFStreamDescriptor *descs[] = {sd};
            hr = MFCreatePresentationDescriptor(1, descs, &pd_);
        }
        if (SUCCEEDED(hr)) {
            stream_ = new (std::nothrow) VCamStream(this, sd);
            if (!stream_) hr = E_OUTOFMEMORY;
        }
        if (sd) sd->Release();
        type->Release();
        return hr;
    }

    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID riid, void **ppv) override {
        if (!ppv) return E_POINTER;
        if (riid == IID_IUnknown || riid == IID_IMFMediaEventGenerator ||
            riid == IID_IMFMediaSource || riid == IID_IMFMediaSourceEx) {
            *ppv = static_cast<IMFMediaSourceEx *>(this);
        } else if (riid == IID_IMFGetService) {
            *ppv = static_cast<IMFGetService *>(this);
        } else if (riid == __uuidof(IKsControl)) {
            *ppv = static_cast<IKsControl *>(this);
        } else {
            LogReject(L"source", riid);
            *ppv = nullptr;
            return E_NOINTERFACE;
        }
        AddRef();
        return S_OK;
    }
    ULONG STDMETHODCALLTYPE AddRef() override {
        return InterlockedIncrement(&refs_);
    }
    ULONG STDMETHODCALLTYPE Release() override {
        LONG n = InterlockedDecrement(&refs_);
        if (n == 0) delete this;
        return n;
    }

    // IMFMediaSourceEx
    HRESULT STDMETHODCALLTYPE GetSourceAttributes(IMFAttributes **a) override {
        if (!a) return E_POINTER;
        if (!source_attrs_) return MF_E_SHUTDOWN;
        *a = source_attrs_;
        source_attrs_->AddRef();
        return S_OK;
    }
    HRESULT STDMETHODCALLTYPE GetStreamAttributes(DWORD id,
                                                  IMFAttributes **a) override {
        if (!a) return E_POINTER;
        if (id != 0) return MF_E_INVALIDSTREAMNUMBER;
        if (!stream_attrs_) return MF_E_SHUTDOWN;
        *a = stream_attrs_;
        stream_attrs_->AddRef();
        return S_OK;
    }
    // No Direct3D here: frames arrive as system memory from another process
    // and are handed on the same way.
    HRESULT STDMETHODCALLTYPE SetD3DManager(IUnknown *) override {
        return E_NOTIMPL;
    }

    // IMFGetService -- nothing to hand out, but the query has to succeed.
    HRESULT STDMETHODCALLTYPE GetService(REFGUID, REFIID riid,
                                         LPVOID *ppv) override {
        if (!ppv) return E_POINTER;
        *ppv = nullptr;
        return MF_E_UNSUPPORTED_SERVICE;
    }

    // IKsControl -- a camera with no adjustable properties still has to answer
    // the property queries rather than refuse the interface.
    HRESULT STDMETHODCALLTYPE KsProperty(PKSPROPERTY, ULONG, LPVOID, ULONG,
                                         ULONG *bytes) override {
        if (bytes) *bytes = 0;
        return HRESULT_FROM_WIN32(ERROR_SET_NOT_FOUND);
    }
    HRESULT STDMETHODCALLTYPE KsMethod(PKSMETHOD, ULONG, LPVOID, ULONG,
                                       ULONG *bytes) override {
        if (bytes) *bytes = 0;
        return HRESULT_FROM_WIN32(ERROR_SET_NOT_FOUND);
    }
    HRESULT STDMETHODCALLTYPE KsEvent(PKSEVENT, ULONG, LPVOID, ULONG,
                                      ULONG *bytes) override {
        if (bytes) *bytes = 0;
        return HRESULT_FROM_WIN32(ERROR_SET_NOT_FOUND);
    }

    HRESULT STDMETHODCALLTYPE GetEvent(DWORD flags, IMFMediaEvent **ev) override {
        return events_ ? events_->GetEvent(flags, ev) : MF_E_SHUTDOWN;
    }
    HRESULT STDMETHODCALLTYPE BeginGetEvent(IMFAsyncCallback *cb,
                                            IUnknown *state) override {
        return events_ ? events_->BeginGetEvent(cb, state) : MF_E_SHUTDOWN;
    }
    HRESULT STDMETHODCALLTYPE EndGetEvent(IMFAsyncResult *r,
                                          IMFMediaEvent **ev) override {
        return events_ ? events_->EndGetEvent(r, ev) : MF_E_SHUTDOWN;
    }
    HRESULT STDMETHODCALLTYPE QueueEvent(MediaEventType met, REFGUID ext,
                                         HRESULT status,
                                         const PROPVARIANT *v) override {
        return events_ ? events_->QueueEventParamVar(met, ext, status, v)
                       : MF_E_SHUTDOWN;
    }

    HRESULT STDMETHODCALLTYPE GetCharacteristics(DWORD *c) override {
        if (!c) return E_POINTER;
        // Live: no seeking, no duration, frames exist only as they arrive.
        *c = MFMEDIASOURCE_IS_LIVE;
        return S_OK;
    }

    HRESULT STDMETHODCALLTYPE
    CreatePresentationDescriptor(IMFPresentationDescriptor **pd) override {
        if (!pd) return E_POINTER;
        if (!pd_) return MF_E_SHUTDOWN;
        return pd_->Clone(pd);
    }

    HRESULT STDMETHODCALLTYPE Start(IMFPresentationDescriptor *pd,
                                    const GUID *timeFormat,
                                    const PROPVARIANT *start) override {
        LogLine(L"source: Start");
        if (timeFormat && *timeFormat != GUID_NULL) {
            return MF_E_UNSUPPORTED_TIME_FORMAT;
        }
        if (!stream_) return MF_E_SHUTDOWN;

        PROPVARIANT pos;
        PropVariantInit(&pos);
        pos.vt = VT_I8;
        pos.hVal.QuadPart = 0;

        // The stream announcement has to reach the consumer before anything
        // will ask for samples, and which event it is depends on whether this
        // is a first start or a restart. Exactly once, and carrying the
        // stream: an announcement with no stream attached is not a weaker
        // version of this event, it is a malformed one, and the consumer
        // rejects the whole start with E_INVALIDARG.
        IUnknown *unk = nullptr;
        HRESULT qi = stream_->QueryInterface(IID_IUnknown, (void **)&unk);
        if (FAILED(qi) || !unk) {
            PropVariantClear(&pos);
            return qi;
        }
        PROPVARIANT sv;
        PropVariantInit(&sv);
        sv.vt = VT_UNKNOWN;
        sv.punkVal = unk;  // PropVariantClear releases this
        events_->QueueEventParamVar(started_once_ ? MEUpdatedStream : MENewStream,
                                    GUID_NULL, S_OK, &sv);
        PropVariantClear(&sv);
        started_once_ = true;

        stream_->SetActive(true);
        stream_->QueueEvent(MEStreamStarted, GUID_NULL, S_OK, &pos);
        QueueEvent(MESourceStarted, GUID_NULL, S_OK, &pos);
        PropVariantClear(&pos);
        (void)pd;
        (void)start;
        return S_OK;
    }

    HRESULT STDMETHODCALLTYPE Stop() override {
        if (!stream_) return MF_E_SHUTDOWN;
        stream_->SetActive(false);
        stream_->QueueEvent(MEStreamStopped, GUID_NULL, S_OK, nullptr);
        QueueEvent(MESourceStopped, GUID_NULL, S_OK, nullptr);
        return S_OK;
    }

    HRESULT STDMETHODCALLTYPE Pause() override {
        if (!stream_) return MF_E_SHUTDOWN;
        stream_->QueueEvent(MEStreamPaused, GUID_NULL, S_OK, nullptr);
        QueueEvent(MESourcePaused, GUID_NULL, S_OK, nullptr);
        return S_OK;
    }

    HRESULT STDMETHODCALLTYPE Shutdown() override {
        if (stream_) {
            stream_->Shutdown();
            stream_->Release();
            stream_ = nullptr;
        }
        if (pd_) {
            pd_->Release();
            pd_ = nullptr;
        }
        if (source_attrs_) {
            source_attrs_->Release();
            source_attrs_ = nullptr;
        }
        if (stream_attrs_) {
            stream_attrs_->Release();
            stream_attrs_ = nullptr;
        }
        if (events_) {
            events_->Shutdown();
            events_->Release();
            events_ = nullptr;
        }
        return S_OK;
    }

private:
    ~VCamSource() {
        Shutdown();
        InterlockedDecrement(&g_locks);
    }

    LONG refs_ = 1;
    IMFMediaEventQueue *events_ = nullptr;
    IMFPresentationDescriptor *pd_ = nullptr;
    IMFAttributes *source_attrs_ = nullptr;
    IMFAttributes *stream_attrs_ = nullptr;
    VCamStream *stream_ = nullptr;
    bool started_once_ = false;
};

HRESULT STDMETHODCALLTYPE VCamStream::GetMediaSource(IMFMediaSource **src) {
    if (!src) return E_POINTER;
    if (!source_) return MF_E_SHUTDOWN;
    return source_->QueryInterface(IID_IMFMediaSource, (void **)src);
}

HRESULT STDMETHODCALLTYPE VCamStream::RequestSample(IUnknown *token) {
    static LONG s_reqs = 0;
    if (InterlockedIncrement(&s_reqs) <= 5) {
        LogLine(L"stream: RequestSample #%ld active=%d", s_reqs, (int)active_);
    }
    if (!events_) return MF_E_SHUTDOWN;
    if (!active_) return MF_E_MEDIA_SOURCE_WRONGSTATE;

    const size_t want = (size_t)kWidth * kHeight * 4;

    uint32_t w = 0, h = 0;
    uint64_t qpc = 0, seq = 0;
    const bool have = reader_.read(frame_, w, h, qpc, seq);

    const uint8_t *pixels = nullptr;
    if (have && w == kWidth && h == kHeight && frame_.size() >= want) {
        pixels = frame_.data();
    } else if (have && w && h) {
        scaled_.resize(want);
        ScaleBgrx(frame_.data(), w, h, scaled_.data(), kWidth, kHeight);
        pixels = scaled_.data();
    }

    IMFMediaBuffer *buf = nullptr;
    HRESULT hr = MFCreateMemoryBuffer((DWORD)want, &buf);
    if (FAILED(hr)) return hr;

    BYTE *dst = nullptr;
    hr = buf->Lock(&dst, nullptr, nullptr);
    if (SUCCEEDED(hr)) {
        if (pixels) {
            memcpy(dst, pixels, want);
        } else {
            // No publisher yet, or an unreadable frame. Black is the honest
            // answer: the camera exists and is streaming, there is simply
            // nothing behind it, and failing the request would make consumers
            // tear the whole stream down over a transient gap.
            memset(dst, 0, want);
        }
        buf->Unlock();
        hr = buf->SetCurrentLength((DWORD)want);
    }

    IMFSample *sample = nullptr;
    if (SUCCEEDED(hr)) hr = MFCreateSample(&sample);
    if (SUCCEEDED(hr)) hr = sample->AddBuffer(buf);
    if (SUCCEEDED(hr)) {
        // Pace by the advertised frame rate rather than by arrival. The
        // capture side is jittery and slower than 30 fps, so timestamping by
        // QPC would hand consumers an irregular stream they would try to
        // correct for; a steady clock with repeated frames is what a webcam
        // that cannot keep up normally looks like.
        const LONGLONG dur = 10000000LL * kFpsDen / kFpsNum;
        hr = sample->SetSampleTime(next_pts_);
        if (SUCCEEDED(hr)) hr = sample->SetSampleDuration(dur);
        next_pts_ += dur;
    }
    if (SUCCEEDED(hr) && token) {
        hr = sample->SetUnknown(MFSampleExtension_Token, token);
    }
    if (SUCCEEDED(hr)) {
        hr = events_->QueueEventParamUnk(MEMediaSample, GUID_NULL, S_OK, sample);
    }

    if (sample) sample->Release();
    buf->Release();
    return hr;
}

// ---------------------------------------------------------------------------
// Activation object.
//
// This, not the source, is what the registered CLSID produces. The frame
// server runs in its own process and has to create the source there, so it
// cannot be handed an existing object -- it creates an IMFActivate and calls
// ActivateObject to get the source on its own side. Registering the source
// directly is the obvious mistake, and it surfaces only as E_NOINTERFACE from
// IMFVirtualCamera::Start for IID_IMFActivate.
//
// IMFActivate derives from IMFAttributes, so all thirty of those come with it.
// They delegate to a store Media Foundation makes for us; the frame server
// does read and write attributes on the activator, so they cannot simply
// return E_NOTIMPL.
// ---------------------------------------------------------------------------

class VCamActivate : public IMFActivate {
public:
    HRESULT Init() { return MFCreateAttributes(&attrs_, 4); }

    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID riid, void **ppv) override {
        if (!ppv) return E_POINTER;
        if (riid == IID_IUnknown || riid == IID_IMFAttributes ||
            riid == IID_IMFActivate) {
            *ppv = static_cast<IMFActivate *>(this);
        } else {
            LogReject(L"activate", riid);
            *ppv = nullptr;
            return E_NOINTERFACE;
        }
        AddRef();
        return S_OK;
    }
    ULONG STDMETHODCALLTYPE AddRef() override {
        return InterlockedIncrement(&refs_);
    }
    ULONG STDMETHODCALLTYPE Release() override {
        LONG n = InterlockedDecrement(&refs_);
        if (n == 0) delete this;
        return n;
    }

    HRESULT STDMETHODCALLTYPE ActivateObject(REFIID riid, void **ppv) override {
        if (!ppv) return E_POINTER;
        if (!source_) {
            VCamSource *src = new (std::nothrow) VCamSource();
            if (!src) return E_OUTOFMEMORY;
            HRESULT hr = src->Init();
            if (FAILED(hr)) {
                src->Release();
                LogLine(L"activate: source Init failed 0x%08x", (unsigned)hr);
                return hr;
            }
            source_ = src;
            LogLine(L"activate: source created");
        }
        return source_->QueryInterface(riid, ppv);
    }

    HRESULT STDMETHODCALLTYPE ShutdownObject() override {
        if (source_) {
            source_->Shutdown();
            source_->Release();
            source_ = nullptr;
        }
        return S_OK;
    }

    // Detaching would hand ownership to the caller. We keep the source tied to
    // this activator instead, so there is exactly one place it dies.
    HRESULT STDMETHODCALLTYPE DetachObject() override { return E_NOTIMPL; }

    // IMFAttributes, delegated wholesale.
    HRESULT STDMETHODCALLTYPE GetItem(REFGUID k, PROPVARIANT *v) override {
        return attrs_->GetItem(k, v);
    }
    HRESULT STDMETHODCALLTYPE GetItemType(REFGUID k, MF_ATTRIBUTE_TYPE *t) override {
        return attrs_->GetItemType(k, t);
    }
    HRESULT STDMETHODCALLTYPE CompareItem(REFGUID k, REFPROPVARIANT v, BOOL *r) override {
        return attrs_->CompareItem(k, v, r);
    }
    HRESULT STDMETHODCALLTYPE Compare(IMFAttributes *o, MF_ATTRIBUTES_MATCH_TYPE t,
                                      BOOL *r) override {
        return attrs_->Compare(o, t, r);
    }
    HRESULT STDMETHODCALLTYPE GetUINT32(REFGUID k, UINT32 *v) override {
        return attrs_->GetUINT32(k, v);
    }
    HRESULT STDMETHODCALLTYPE GetUINT64(REFGUID k, UINT64 *v) override {
        return attrs_->GetUINT64(k, v);
    }
    HRESULT STDMETHODCALLTYPE GetDouble(REFGUID k, double *v) override {
        return attrs_->GetDouble(k, v);
    }
    HRESULT STDMETHODCALLTYPE GetGUID(REFGUID k, GUID *v) override {
        return attrs_->GetGUID(k, v);
    }
    HRESULT STDMETHODCALLTYPE GetStringLength(REFGUID k, UINT32 *n) override {
        return attrs_->GetStringLength(k, n);
    }
    HRESULT STDMETHODCALLTYPE GetString(REFGUID k, LPWSTR s, UINT32 n,
                                        UINT32 *len) override {
        return attrs_->GetString(k, s, n, len);
    }
    HRESULT STDMETHODCALLTYPE GetAllocatedString(REFGUID k, LPWSTR *s,
                                                 UINT32 *len) override {
        return attrs_->GetAllocatedString(k, s, len);
    }
    HRESULT STDMETHODCALLTYPE GetBlobSize(REFGUID k, UINT32 *n) override {
        return attrs_->GetBlobSize(k, n);
    }
    HRESULT STDMETHODCALLTYPE GetBlob(REFGUID k, UINT8 *b, UINT32 n,
                                      UINT32 *got) override {
        return attrs_->GetBlob(k, b, n, got);
    }
    HRESULT STDMETHODCALLTYPE GetAllocatedBlob(REFGUID k, UINT8 **b,
                                               UINT32 *n) override {
        return attrs_->GetAllocatedBlob(k, b, n);
    }
    HRESULT STDMETHODCALLTYPE GetUnknown(REFGUID k, REFIID riid, LPVOID *v) override {
        return attrs_->GetUnknown(k, riid, v);
    }
    HRESULT STDMETHODCALLTYPE SetItem(REFGUID k, REFPROPVARIANT v) override {
        return attrs_->SetItem(k, v);
    }
    HRESULT STDMETHODCALLTYPE DeleteItem(REFGUID k) override {
        return attrs_->DeleteItem(k);
    }
    HRESULT STDMETHODCALLTYPE DeleteAllItems() override {
        return attrs_->DeleteAllItems();
    }
    HRESULT STDMETHODCALLTYPE SetUINT32(REFGUID k, UINT32 v) override {
        return attrs_->SetUINT32(k, v);
    }
    HRESULT STDMETHODCALLTYPE SetUINT64(REFGUID k, UINT64 v) override {
        return attrs_->SetUINT64(k, v);
    }
    HRESULT STDMETHODCALLTYPE SetDouble(REFGUID k, double v) override {
        return attrs_->SetDouble(k, v);
    }
    HRESULT STDMETHODCALLTYPE SetGUID(REFGUID k, REFGUID v) override {
        return attrs_->SetGUID(k, v);
    }
    HRESULT STDMETHODCALLTYPE SetString(REFGUID k, LPCWSTR v) override {
        return attrs_->SetString(k, v);
    }
    HRESULT STDMETHODCALLTYPE SetBlob(REFGUID k, const UINT8 *b, UINT32 n) override {
        return attrs_->SetBlob(k, b, n);
    }
    HRESULT STDMETHODCALLTYPE SetUnknown(REFGUID k, IUnknown *v) override {
        return attrs_->SetUnknown(k, v);
    }
    HRESULT STDMETHODCALLTYPE LockStore() override { return attrs_->LockStore(); }
    HRESULT STDMETHODCALLTYPE UnlockStore() override { return attrs_->UnlockStore(); }
    HRESULT STDMETHODCALLTYPE GetCount(UINT32 *n) override {
        return attrs_->GetCount(n);
    }
    HRESULT STDMETHODCALLTYPE GetItemByIndex(UINT32 i, GUID *k,
                                             PROPVARIANT *v) override {
        return attrs_->GetItemByIndex(i, k, v);
    }
    HRESULT STDMETHODCALLTYPE CopyAllItems(IMFAttributes *to) override {
        return attrs_->CopyAllItems(to);
    }

private:
    // Releases its reference to the source but does not shut it down. The
    // caller owns that decision and makes it through ShutdownObject: Media
    // Foundation routinely releases the activator as soon as it holds the
    // source, and tearing the source down here leaves the frame server
    // holding an object that answers MF_E_SHUTDOWN to everything.
    ~VCamActivate() {
        if (source_) source_->Release();
        if (attrs_) attrs_->Release();
    }

    LONG refs_ = 1;
    IMFAttributes *attrs_ = nullptr;
    VCamSource *source_ = nullptr;
};

// ---------------------------------------------------------------------------
// COM class factory and DLL exports.
// ---------------------------------------------------------------------------

class VCamFactory : public IClassFactory {
public:
    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID riid, void **ppv) override {
        if (!ppv) return E_POINTER;
        if (riid == IID_IUnknown || riid == IID_IClassFactory) {
            *ppv = static_cast<IClassFactory *>(this);
            AddRef();
            return S_OK;
        }
        *ppv = nullptr;
        return E_NOINTERFACE;
    }
    ULONG STDMETHODCALLTYPE AddRef() override {
        return InterlockedIncrement(&refs_);
    }
    ULONG STDMETHODCALLTYPE Release() override {
        LONG n = InterlockedDecrement(&refs_);
        if (n == 0) delete this;
        return n;
    }

    HRESULT STDMETHODCALLTYPE CreateInstance(IUnknown *outer, REFIID riid,
                                             void **ppv) override {
        if (outer) return CLASS_E_NOAGGREGATION;
        // The activator, not the source: the frame server creates this and
        // asks it for the source itself.
        VCamActivate *act = new (std::nothrow) VCamActivate();
        if (!act) return E_OUTOFMEMORY;
        HRESULT hr = act->Init();
        if (SUCCEEDED(hr)) hr = act->QueryInterface(riid, ppv);
        act->Release();
        return hr;
    }

    HRESULT STDMETHODCALLTYPE LockServer(BOOL lock) override {
        if (lock) InterlockedIncrement(&g_locks);
        else InterlockedDecrement(&g_locks);
        return S_OK;
    }

private:
    LONG refs_ = 1;
};

BOOL WINAPI DllMain(HINSTANCE inst, DWORD reason, LPVOID) {
    if (reason == DLL_PROCESS_ATTACH) {
        g_module = inst;
        DisableThreadLibraryCalls(inst);
    }
    return TRUE;
}

STDAPI DllGetClassObject(REFCLSID clsid, REFIID riid, void **ppv) {
    if (clsid != CLSID_VCamSource) return CLASS_E_CLASSNOTAVAILABLE;
    VCamFactory *f = new (std::nothrow) VCamFactory();
    if (!f) return E_OUTOFMEMORY;
    HRESULT hr = f->QueryInterface(riid, ppv);
    f->Release();
    return hr;
}

STDAPI DllCanUnloadNow() {
    return g_locks == 0 ? S_OK : S_FALSE;
}

static LONG RegisterUnder(HKEY root) {
    wchar_t path[MAX_PATH];
    if (!GetModuleFileNameW(g_module, path, MAX_PATH)) {
        return (LONG)GetLastError();
    }

    wchar_t clsid[64];
    if (StringFromGUID2(CLSID_VCamSource, clsid, 64) == 0) return ERROR_INVALID_DATA;

    wchar_t key[160];
    swprintf_s(key, L"Software\\Classes\\CLSID\\%s\\InprocServer32", clsid);

    HKEY h = nullptr;
    LONG rc = RegCreateKeyExW(root, key, 0, nullptr, 0, KEY_WRITE, nullptr, &h,
                              nullptr);
    if (rc != ERROR_SUCCESS) return rc;

    rc = RegSetValueExW(h, nullptr, 0, REG_SZ, (const BYTE *)path,
                        (DWORD)((wcslen(path) + 1) * sizeof(wchar_t)));
    if (rc == ERROR_SUCCESS) {
        const wchar_t *model = L"Both";
        rc = RegSetValueExW(h, L"ThreadingModel", 0, REG_SZ,
                            (const BYTE *)model,
                            (DWORD)((wcslen(model) + 1) * sizeof(wchar_t)));
    }
    RegCloseKey(h);
    return rc;
}

// Registers machine-wide, and that is not a preference.
//
// IMFVirtualCamera::Start hands the camera to the Frame Server service, which
// runs under its own account and never sees HKEY_CURRENT_USER. Registered per
// user, the CLSID resolves in our process -- the activator even runs -- and
// then Start fails with ERROR_PATH_NOT_FOUND from the service, which cannot
// find a class that, from where it is standing, does not exist.
//
// HKCU is still attempted as a fallback, because a per-user registration is
// enough for anything that activates the source in-process, and a clear
// failure later beats refusing to register at all.
STDAPI DllRegisterServer() {
    LONG rc = RegisterUnder(HKEY_LOCAL_MACHINE);
    if (rc == ERROR_SUCCESS) {
        LogLine(L"registered under HKLM");
        return S_OK;
    }
    const LONG machine_rc = rc;
    rc = RegisterUnder(HKEY_CURRENT_USER);
    if (rc == ERROR_SUCCESS) {
        LogLine(L"HKLM refused (%ld), registered under HKCU only", machine_rc);
        // Deliberately a success the caller can distinguish: registration
        // happened, but not the kind the frame server can use.
        return S_FALSE;
    }
    return HRESULT_FROM_WIN32(rc);
}

STDAPI DllUnregisterServer() {
    wchar_t clsid[64];
    if (StringFromGUID2(CLSID_VCamSource, clsid, 64) == 0) return E_FAIL;
    wchar_t key[160];
    swprintf_s(key, L"Software\\Classes\\CLSID\\%s", clsid);

    LONG a = RegDeleteTreeW(HKEY_LOCAL_MACHINE, key);
    LONG b = RegDeleteTreeW(HKEY_CURRENT_USER, key);
    const bool ok = (a == ERROR_SUCCESS || a == ERROR_FILE_NOT_FOUND) &&
                    (b == ERROR_SUCCESS || b == ERROR_FILE_NOT_FOUND);
    return ok ? S_OK : E_FAIL;
}
