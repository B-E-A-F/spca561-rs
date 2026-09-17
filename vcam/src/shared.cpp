// Reader side of the shared frame mapping, plus the seqlock protocol.
//
// Only the reader lives here; the writer is the Rust program. Keeping the
// protocol in one file that both the DLL and the host compile means the two
// cannot drift apart in how they interpret the header.
#include "shared.h"

#include <windows.h>

namespace vcam {

FrameReader::~FrameReader() {
    close();
}

void FrameReader::close() {
    if (view_) {
        UnmapViewOfFile(view_);
        view_ = nullptr;
    }
    if (mapping_) {
        CloseHandle(mapping_);
        mapping_ = nullptr;
    }
}

bool FrameReader::open() {
    if (view_) {
        return true;
    }
    // OPEN_EXISTING rather than OPEN_ALWAYS: the capture program owns this
    // file, and a reader that created it would paper over the publisher not
    // running with a block of zeroes indistinguishable from a black frame.
    // Shared for write too, because the publisher is writing it continuously.
    HANDLE file = CreateFileW(VCAM_FILE_PATH, GENERIC_READ,
                              FILE_SHARE_READ | FILE_SHARE_WRITE, nullptr,
                              OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, nullptr);
    if (file == INVALID_HANDLE_VALUE) {
        last_error_ = GetLastError();
        return false;
    }

    mapping_ = CreateFileMappingW(file, nullptr, PAGE_READONLY, 0, 0, nullptr);
    // The mapping keeps the file alive; the handle is not needed past this.
    CloseHandle(file);
    if (!mapping_) {
        last_error_ = GetLastError();
        return false;
    }

    view_ = MapViewOfFile(mapping_, FILE_MAP_READ, 0, 0, VCAM_MAPPING_SIZE);
    if (!view_) {
        last_error_ = GetLastError();
        CloseHandle(mapping_);
        mapping_ = nullptr;
        return false;
    }
    return true;
}

bool FrameReader::read(std::vector<uint8_t> &out, uint32_t &width,
                       uint32_t &height, uint64_t &qpc, uint64_t &seq) {
    if (!view_ && !open()) {
        return false;
    }
    auto *hdr = static_cast<const VcamHeader *>(view_);
    if (hdr->magic != VCAM_MAGIC || hdr->version != VCAM_VERSION) {
        return false;
    }

    const auto *pixels = static_cast<const uint8_t *>(view_) + sizeof(VcamHeader);

    // Seqlock read. A handful of attempts is plenty: the writer holds the odd
    // state only for the length of one memcpy, so losing this race repeatedly
    // means something is wrong rather than merely busy, and blocking here
    // would stall whichever application is hosting us.
    for (int attempt = 0; attempt < 8; ++attempt) {
        const uint64_t before = hdr->seq;
        if (before & 1) {
            YieldProcessor();
            continue;
        }
        const uint32_t w = hdr->width;
        const uint32_t h = hdr->height;
        const uint32_t stride = hdr->stride;
        if (w == 0 || h == 0 || w > VCAM_MAX_WIDTH || h > VCAM_MAX_HEIGHT ||
            stride < w * 4) {
            return false;
        }
        const size_t bytes = (size_t)stride * h;
        if (sizeof(VcamHeader) + bytes > VCAM_MAPPING_SIZE) {
            return false;
        }

        out.resize(bytes);
        memcpy(out.data(), pixels, bytes);
        const uint64_t stamp = hdr->qpc;

        // Reading seq again after the copy is what makes this safe: unchanged
        // and even means nothing was written while we were copying.
        MemoryBarrier();
        if (hdr->seq == before) {
            width = w;
            height = h;
            qpc = stamp;
            seq = before;
            return true;
        }
    }
    return false;
}

} // namespace vcam
