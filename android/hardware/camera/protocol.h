#pragma once

#include <poll.h>
#include <sys/socket.h>

#include <algorithm>
#include <array>
#include <atomic>
#include <cerrno>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <limits>
#include <optional>

namespace droidloom::camera {

constexpr int kWidth = 640;
constexpr int kHeight = 480;
constexpr size_t kFrameBytes = kWidth * kHeight * 4;
constexpr uint16_t kVersion = 1;
constexpr std::chrono::seconds kIoTimeout{5};

enum class Operation : uint16_t { Catalogue = 1, Open = 2 };
enum class Kind : uint16_t { Catalogue = 1, Opened = 2, Frame = 3, Error = 4 };
enum class IoResult { Ok, Closed, Timeout, Cancelled, Error };

struct Response {
    Kind kind;
    uint32_t length;
    uint32_t status;
    uint64_t timestamp;
};

inline uint64_t littleEndian(const uint8_t* bytes, size_t count) {
    uint64_t value = 0;
    for (size_t i = 0; i < count; ++i) value |= uint64_t{bytes[i]} << (i * 8);
    return value;
}

inline std::array<uint8_t, 16> request(Operation operation, uint32_t cameraId) {
    std::array<uint8_t, 16> bytes{'D', 'C', 'A', 'M', 1, 0};
    const auto opcode = static_cast<uint16_t>(operation);
    bytes[6] = opcode & 0xff;
    bytes[7] = opcode >> 8;
    for (size_t i = 0; i < 4; ++i) bytes[8 + i] = (cameraId >> (i * 8)) & 0xff;
    return bytes;
}

inline std::optional<Response> response(const std::array<uint8_t, 24>& bytes) {
    if (std::memcmp(bytes.data(), "DCAR", 4) != 0 || littleEndian(bytes.data() + 4, 2) != kVersion) {
        return std::nullopt;
    }
    Response value{
        static_cast<Kind>(littleEndian(bytes.data() + 6, 2)),
        static_cast<uint32_t>(littleEndian(bytes.data() + 8, 4)),
        static_cast<uint32_t>(littleEndian(bytes.data() + 12, 4)),
        littleEndian(bytes.data() + 16, 8),
    };
    if (value.kind == Kind::Error) {
        if (value.length != 0 || value.timestamp != 0 || value.status < 1 || value.status > 5) {
            return std::nullopt;
        }
    } else {
        if (value.status != 0) return std::nullopt;
        switch (value.kind) {
            case Kind::Catalogue:
                if (value.length != 4 || value.timestamp != 0) return std::nullopt;
                break;
            case Kind::Opened:
                if (value.length != 0 || value.timestamp != 0) return std::nullopt;
                break;
            case Kind::Frame:
                if (value.length != kFrameBytes || value.timestamp == 0 ||
                    value.timestamp > static_cast<uint64_t>(std::numeric_limits<int64_t>::max())) {
                    return std::nullopt;
                }
                break;
            default:
                return std::nullopt;
        }
    }
    return value;
}

inline IoResult waitSocket(int fd, short events,
                           std::chrono::steady_clock::time_point deadline,
                           const std::atomic_bool& stopped) {
    while (!stopped.load()) {
        const auto remaining = deadline - std::chrono::steady_clock::now();
        if (remaining <= std::chrono::steady_clock::duration::zero()) return IoResult::Timeout;
        const int milliseconds = static_cast<int>(std::clamp<int64_t>(
            std::chrono::duration_cast<std::chrono::milliseconds>(remaining).count(), 1, 100));
        pollfd descriptor{fd, events, 0};
        const int result = poll(&descriptor, 1, milliseconds);
        if (result < 0) {
            if (errno == EINTR) continue;
            return IoResult::Error;
        }
        if (result == 0) continue;
        if (descriptor.revents & POLLNVAL) return IoResult::Error;
        if (descriptor.revents & (events | POLLHUP | POLLERR)) return IoResult::Ok;
    }
    return IoResult::Cancelled;
}

template <typename Transfer>
IoResult transfer(size_t count, short events, int fd, const std::atomic_bool& stopped,
                  std::chrono::milliseconds timeout, Transfer operation) {
    const auto deadline = std::chrono::steady_clock::now() + timeout;
    size_t offset = 0;
    while (offset < count) {
        if (stopped.load()) return IoResult::Cancelled;
        if (std::chrono::steady_clock::now() >= deadline) return IoResult::Timeout;
        const ssize_t bytes = operation(offset, count - offset);
        if (bytes == 0) return IoResult::Closed;
        if (bytes > 0) {
            offset += static_cast<size_t>(bytes);
            continue;
        }
        if (errno == EINTR) continue;
        if (errno != EAGAIN && errno != EWOULDBLOCK) return IoResult::Error;
        const IoResult result = waitSocket(fd, events, deadline, stopped);
        if (result != IoResult::Ok) return result;
    }
    return IoResult::Ok;
}

inline IoResult readExact(int fd, uint8_t* bytes, size_t count, const std::atomic_bool& stopped,
                          std::chrono::milliseconds timeout = kIoTimeout) {
    return transfer(count, POLLIN, fd, stopped, timeout, [&](size_t offset, size_t remaining) {
        return recv(fd, bytes + offset, remaining, MSG_DONTWAIT);
    });
}

inline IoResult writeAll(int fd, const uint8_t* bytes, size_t count,
                         const std::atomic_bool& stopped,
                         std::chrono::milliseconds timeout = kIoTimeout) {
    return transfer(count, POLLOUT, fd, stopped, timeout, [&](size_t offset, size_t remaining) {
        return send(fd, bytes + offset, remaining, MSG_DONTWAIT | MSG_NOSIGNAL);
    });
}

inline bool copyFrame(const uint8_t* packed, size_t bytes, void* pixels,
                      int width, int height, int stridePixels) {
    if (bytes != kFrameBytes || packed == nullptr || pixels == nullptr ||
        width != kWidth || height != kHeight || stridePixels < width ||
        static_cast<size_t>(stridePixels) > std::numeric_limits<size_t>::max() / (kHeight * 4)) {
        return false;
    }
    auto* destination = static_cast<uint8_t*>(pixels);
    for (int row = 0; row < kHeight; ++row) {
        std::memcpy(destination + static_cast<size_t>(row) * stridePixels * 4,
                    packed + static_cast<size_t>(row) * kWidth * 4, kWidth * 4);
    }
    return true;
}

}  // namespace droidloom::camera
