/* SPDX-License-Identifier: GPL-3.0-or-later */
#include "DroidloomImportRetention.h"
#include <cassert>
#include <fcntl.h>
#include <set>
#include <sys/eventfd.h>
#include <unistd.h>
#include <unordered_map>
#include <utility>

using android::DroidloomImportUse;
using android::trimDroidloomImports;
constexpr uint64_t MiB = 1024 * 1024;
struct Entry {
    DroidloomImportUse use;
    int fd = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
    explicit Entry(DroidloomImportUse u) : use(u) { assert(fd >= 0); }
    Entry(Entry&& e) noexcept : use(e.use), fd(std::exchange(e.fd, -1)) {}
    ~Entry() { if (fd >= 0) close(fd); }
};
using Cache = std::unordered_map<uint64_t, Entry>;
using Scene = std::map<uint64_t, DroidloomImportUse>;
DroidloomImportUse use(uint32_t layer = 1, uint32_t width = 1920,
                      uint64_t bytes = 8 * MiB, uint64_t serial = 1) {
    return {layer, width, 1080, 1, bytes, serial};
}

void resizeAndRelease() {
    Cache cache;
    std::set<uint64_t> busy;
    auto retire = [&](uint64_t id) { return !busy.contains(id); };
    for (uint64_t frame = 1; frame <= 2000; ++frame) {
        auto u = use(1, 800 + frame, 12 * MiB, frame);
        cache.try_emplace(frame, u);
        Scene scene{{frame, u}};
        // The preceding frame is still read by the GPU during this resize.
        busy = {frame - 1};
        const bool pending = trimDroidloomImports(cache, scene, retire);
        assert(pending == (frame > 1));
        assert(cache.size() == (frame == 1 ? 1 : 2));
        if (frame > 1) {
            const int fd = cache.at(frame - 1).fd;
            assert(fcntl(fd, F_GETFD) >= 0);
            busy.clear();
            // Same scene: release-driven cleanup needs no new content frame.
            assert(!trimDroidloomImports(cache, scene, retire));
            assert(fcntl(fd, F_GETFD) == -1);
            assert(cache.size() == 1);
        }
    }
}

void rotatingPoolStaysImported() {
    Cache cache;
    size_t retirements = 0;
    auto retire = [&](uint64_t) { ++retirements; return true; };
    for (uint64_t frame = 1; frame <= 2000; ++frame) {
        const uint64_t id = frame % 4 + 1;
        auto u = use(1, 3392, 32 * MiB, frame);
        cache.try_emplace(id, u);
        assert(!trimDroidloomImports(cache, Scene{{id, u}}, retire));
    }
    assert(cache.size() == 4 && retirements == 0);
}

void pressureAndOwnership() {
    Cache cache;
    Scene current;
    for (uint64_t layer = 1; layer <= 8; ++layer) {
        for (uint64_t slot = 0; slot <= 4; ++slot) {
            const uint64_t id = layer * 10 + slot;
            auto u = use(layer, 1920, 16 * MiB, slot);
            cache.try_emplace(id, u);
            if (!slot) current.emplace(id, u);
        }
    }
    assert(!trimDroidloomImports(cache, current, [](uint64_t) { return true; }));
    assert(cache.size() == current.size() + 6); // 96 MiB spare budget, across layers.
    for (const auto& [id, u] : current) assert(cache.contains(id));

    cache.clear(); current.clear();
    for (uint64_t layer = 1; layer <= 8; ++layer) {
        for (uint64_t slot = 0; slot <= 4; ++slot) {
            const uint64_t id = layer * 10 + slot;
            auto u = use(layer, 16, 4096, slot);
            cache.try_emplace(id, u);
            if (!slot) current.emplace(id, u);
        }
    }
    assert(!trimDroidloomImports(cache, current, [](uint64_t) { return true; }));
    assert(cache.size() == current.size() + 12); // Count bound independently of bytes.
    // Removed layers and unknown/oversized allocation sizes never stay warm.
    cache.clear(); current.clear();
    for (uint64_t id = 1; id <= 5; ++id) cache.try_emplace(id, use(1, 1920, UINT64_MAX));
    current.emplace(1, use(1, 1920, UINT64_MAX));
    assert(!trimDroidloomImports(cache, current, [](uint64_t) { return true; }));
    assert(cache.size() == 1); // Never evict the current allocation, however large.
    assert(!trimDroidloomImports(cache, {}, [](uint64_t) { return true; }));
    assert(cache.empty());
}

void newestCompatibleSpares() {
    Cache cache;
    for (uint64_t id = 1; id <= 8; ++id) cache.try_emplace(id, use(1, 1920, MiB, id));
    cache.try_emplace(9, use(1, 800, MiB, 9)); // Obsolete dimensions, though most recent.
    cache.try_emplace(10, use(2, 1920, MiB, 10)); // Removed layer.
    cache.try_emplace(11, use(1, 1920, 0, 11)); // Unknown size.
    auto wrongFormat = use(1, 1920, MiB, 12); wrongFormat.format = 2;
    cache.try_emplace(12, wrongFormat);
    assert(!trimDroidloomImports(cache, Scene{{1, use()}}, [](uint64_t) { return true; }));
    assert(cache.size() == 4);
    for (uint64_t id : {1, 6, 7, 8}) assert(cache.contains(id));
}

int main() {
    resizeAndRelease();
    rotatingPoolStaysImported();
    pressureAndOwnership();
    newestCompatibleSpares();
}
