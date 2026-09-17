// Shared-memory frame handover between the capture program and the virtual
// camera's media source.
//
// They are separate processes by necessity: the media source is an in-process
// COM server, so Windows loads it inside whichever application is opening the
// camera -- Teams, the Camera app, a browser. It cannot be part of the Rust
// program, and it cannot call into it. A block of shared memory is the whole
// contract between them.
//
// Layout is a header followed by the pixels, in one mapping:
//
//     [ VcamHeader ][ width * height * 4 bytes of BGRX ]
//
// Pixels are BGRX, which is what MFVideoFormat_RGB32 means in memory and also
// exactly what the Rust side already has: its u32 is (r<<16)|(g<<8)|b, which
// on a little-endian machine is the bytes b, g, r, x in that order. No
// conversion, no copy beyond the one into the mapping.
#pragma once

#include <cstdint>

#ifdef __cplusplus
extern "C" {
#endif

// "SPCA" little-endian. A reader that finds anything else is looking at
// uninitialised or foreign memory and should not trust the rest.
#define VCAM_MAGIC 0x41435053u
#define VCAM_VERSION 1u

// Backed by a real file rather than a named page-file mapping, and that is the
// whole point.
//
// A named mapping lives in a session's object namespace: Local\ is visible
// only within one session, and the media source is hosted by the Frame Server,
// which runs in its own. The publisher would create the mapping in the user
// session and the source would look for it in the service session and never
// find it -- a camera that starts, streams, and shows black forever. Global\
// crosses sessions but needs SeCreateGlobalPrivilege, which would mean running
// the capture program elevated just to show a picture.
//
// A file has no session. Both processes open the same path and the question
// does not arise.
#define VCAM_FILE_DIR L"C:\\ProgramData\\spca561"
#define VCAM_FILE_PATH L"C:\\ProgramData\\spca561\\frame.bin"

// Largest frame the mapping can hold. The camera's biggest mode is 352x288,
// but the mapping is sized once and never resized, so leave room to grow
// rather than tie the ABI to the current mode table.
#define VCAM_MAX_WIDTH 1920u
#define VCAM_MAX_HEIGHT 1080u

typedef struct VcamHeader {
    uint32_t magic;
    uint32_t version;
    uint32_t width;
    uint32_t height;
    // Bytes per row. Always width * 4 today, but carried explicitly so a
    // padded layout does not become a silent reinterpretation later.
    uint32_t stride;
    uint32_t reserved;

    // Seqlock. The writer makes this odd before touching pixels and even
    // after, so a reader that sees the same even value either side of its copy
    // knows the frame was not rewritten underneath it. Chosen over a mutex
    // because the reader lives inside somebody else's application, and a
    // reader that stalls or dies must never be able to block the writer.
    volatile uint64_t seq;

    // QPC ticks when the frame was published. The media source turns this into
    // a presentation timestamp rather than inventing its own clock, so
    // capture jitter reaches the consumer as it really happened.
    volatile uint64_t qpc;
} VcamHeader;

#define VCAM_MAPPING_SIZE \
    (sizeof(VcamHeader) + (size_t)VCAM_MAX_WIDTH * VCAM_MAX_HEIGHT * 4)

#ifdef __cplusplus
}

#include <vector>

namespace vcam {

/// Reads published frames out of the mapping.
///
/// Opens lazily and survives the publisher not being there: the camera can be
/// activated before the capture program starts, or outlive it, and neither
/// should be an error the consumer sees. `read` simply fails until frames
/// appear, and the caller decides what to show in the meantime.
class FrameReader {
public:
    FrameReader() = default;
    ~FrameReader();
    FrameReader(const FrameReader &) = delete;
    FrameReader &operator=(const FrameReader &) = delete;

    bool open();
    void close();

    /// Copies the newest complete frame. False if no publisher, or if the
    /// frame could not be read cleanly.
    bool read(std::vector<uint8_t> &out, uint32_t &width, uint32_t &height,
              uint64_t &qpc, uint64_t &seq);

private:
    void *mapping_ = nullptr;
    void *view_ = nullptr;

public:
    /// Why the last open failed. ERROR_FILE_NOT_FOUND here with the capture
    /// program demonstrably running means the name is not visible from this
    /// process -- which is the interesting case, because the media source may
    /// be hosted in a different session than the publisher.
    unsigned long last_error_ = 0;
};

} // namespace vcam
#endif
