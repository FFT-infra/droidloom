#include "protocol.h"

#include <sys/socket.h>
#include <unistd.h>

#include <cstdio>
#include <cstdlib>
#include <thread>
#include <vector>

using namespace droidloom::camera;

namespace {

void require(bool condition, const char* message) {
    if (!condition) {
        std::fprintf(stderr, "FAIL: %s\n", message);
        std::exit(EXIT_FAILURE);
    }
}

std::array<uint8_t, 24> packet(Kind kind, uint32_t length, uint32_t status, uint64_t timestamp) {
    std::array<uint8_t, 24> bytes{'D', 'C', 'A', 'R', 1, 0};
    bytes[6] = static_cast<uint16_t>(kind);
    for (size_t i = 0; i < 4; ++i) {
        bytes[8 + i] = (length >> (i * 8)) & 0xff;
        bytes[12 + i] = (status >> (i * 8)) & 0xff;
    }
    for (size_t i = 0; i < 8; ++i) bytes[16 + i] = (timestamp >> (i * 8)) & 0xff;
    return bytes;
}

struct Pair {
    Pair() { require(socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, fd) == 0, "socketpair"); }
    ~Pair() { close(fd[0]); close(fd[1]); }
    int fd[2];
};

void wireContract() {
    const auto query = request(Operation::Open, 1);
    const std::array<uint8_t, 16> expected{'D', 'C', 'A', 'M', 1, 0, 2, 0, 1, 0, 0, 0, 0, 0, 0, 0};
    require(query == expected, "request must match Rust bridge byte contract");
    require(response(packet(Kind::Catalogue, 4, 0, 0)).has_value(), "catalogue header");
    require(response(packet(Kind::Opened, 0, 0, 0)).has_value(), "opened header");
    const auto frame = response(packet(Kind::Frame, kFrameBytes, 0, 0x0102030405060708ULL));
    require(frame && frame->timestamp == 0x0102030405060708ULL, "little-endian frame timestamp");
    for (uint32_t error = 1; error <= 5; ++error) {
        require(response(packet(Kind::Error, 0, error, 0)).has_value(), "defined host errors");
    }
}

void rejectsMalformedHeaders() {
    auto valid = packet(Kind::Frame, kFrameBytes, 0, 1);
    require(response(valid).has_value(), "malformed test positive control");
    auto bad = valid;
    bad[0] = 'X';
    require(!response(bad), "bad magic");
    bad = valid;
    bad[4] = 2;
    require(!response(bad), "unsupported protocol version");
    require(!response(packet(static_cast<Kind>(99), 0, 0, 0)), "unknown packet type");
    require(!response(packet(Kind::Frame, kFrameBytes - 1, 0, 1)), "short declared frame");
    require(!response(packet(Kind::Frame, UINT32_MAX, 0, 1)), "oversized frame");
    require(!response(packet(Kind::Frame, kFrameBytes, 0, 0)), "zero timestamp");
    require(!response(packet(Kind::Frame, kFrameBytes, 0, UINT64_MAX)), "timestamp signed overflow");
    require(!response(packet(Kind::Frame, kFrameBytes, 1, 1)), "success packet with error status");
    require(!response(packet(Kind::Opened, 4, 0, 0)), "unexpected opened payload");
    require(!response(packet(Kind::Catalogue, 4, 0, 1)), "unexpected catalogue timestamp");
    require(!response(packet(Kind::Error, 0, 0, 0)), "error with success status");
    require(!response(packet(Kind::Error, 0, 6, 0)), "unknown error status");
    require(!response(packet(Kind::Error, 1, 1, 0)), "unexpected error payload");
}

void respectsDestinationStride() {
    std::vector<uint8_t> frame(kFrameBytes);
    for (size_t i = 0; i < frame.size(); ++i) frame[i] = static_cast<uint8_t>(i * 37);
    constexpr int stride = kWidth + 11;
    std::vector<uint8_t> destination(static_cast<size_t>(stride) * kHeight * 4 + 16, 0xa5);
    require(copyFrame(frame.data(), frame.size(), destination.data(), kWidth, kHeight, stride),
            "padded destination accepts packed RGBA");
    for (int row = 0; row < kHeight; ++row) {
        require(std::memcmp(destination.data() + static_cast<size_t>(row) * stride * 4,
                            frame.data() + static_cast<size_t>(row) * kWidth * 4, kWidth * 4) == 0,
                "row content and order preserved");
        for (int byte = kWidth * 4; byte < stride * 4; ++byte) {
            require(destination[static_cast<size_t>(row) * stride * 4 + byte] == 0xa5,
                    "row padding preserved");
        }
    }
    for (size_t i = destination.size() - 16; i < destination.size(); ++i) {
        require(destination[i] == 0xa5, "end guard preserved");
    }
    require(!copyFrame(frame.data(), frame.size() - 1, destination.data(), kWidth, kHeight, stride),
            "truncated input rejected");
    require(!copyFrame(frame.data(), frame.size(), destination.data(), kWidth, kHeight, kWidth - 1),
            "short destination stride rejected");
    require(!copyFrame(frame.data(), frame.size(), destination.data(), kWidth, kHeight - 1, stride),
            "wrong destination height rejected");
    require(!copyFrame(frame.data(), frame.size(), nullptr, kWidth, kHeight, stride),
            "null destination rejected");
}

void readsFragmentedPacket() {
    Pair pair;
    const std::atomic_bool stopped{false};
    const auto expected = packet(Kind::Frame, kFrameBytes, 0, 123);
    std::thread writer([&] {
        for (uint8_t byte : expected) {
            require(writeAll(pair.fd[1], &byte, 1, stopped) == IoResult::Ok, "fragment send");
        }
    });
    std::array<uint8_t, 24> actual{};
    require(readExact(pair.fd[0], actual.data(), actual.size(), stopped) == IoResult::Ok,
            "fragmented header reassembled");
    writer.join();
    require(actual == expected, "fragmented packet bytes preserved");
}

void rejectsTruncatedPacket() {
    Pair pair;
    const std::atomic_bool stopped{false};
    std::array<uint8_t, 24> bytes{};
    require(writeAll(pair.fd[1], bytes.data(), 7, stopped) == IoResult::Ok, "short packet write");
    shutdown(pair.fd[1], SHUT_WR);
    require(readExact(pair.fd[0], bytes.data(), bytes.size(), stopped) == IoResult::Closed,
            "short packet cannot become a frame");
}

void boundsSilentPeer() {
    Pair pair;
    const std::atomic_bool stopped{false};
    uint8_t byte = 0;
    const auto start = std::chrono::steady_clock::now();
    require(readExact(pair.fd[0], &byte, 1, stopped, std::chrono::milliseconds(20)) == IoResult::Timeout,
            "silent peer deadline");
    require(std::chrono::steady_clock::now() - start < std::chrono::seconds(1),
            "read must not block on a blocking socket");
}

void cancellationReleasesReader() {
    Pair pair;
    std::atomic_bool stopped{false};
    IoResult result = IoResult::Ok;
    std::thread reader([&] {
        uint8_t byte = 0;
        result = readExact(pair.fd[0], &byte, 1, stopped);
    });
    stopped.store(true);
    shutdown(pair.fd[0], SHUT_RDWR);
    reader.join();
    require(result == IoResult::Cancelled, "cancelled stream releases blocked reader");
}

void boundsStalledWriter() {
    Pair pair;
    const std::atomic_bool stopped{false};
    std::vector<uint8_t> payload(kFrameBytes, 0x55);
    require(writeAll(pair.fd[0], payload.data(), payload.size(), stopped,
                     std::chrono::milliseconds(20)) == IoResult::Timeout,
            "non-reading peer cannot block writes forever");
}

void closedPeerCannotSignalProcess() {
    Pair pair;
    const std::atomic_bool stopped{false};
    shutdown(pair.fd[1], SHUT_RDWR);
    const uint8_t byte = 1;
    require(writeAll(pair.fd[0], &byte, 1, stopped) == IoResult::Error,
            "closed peer returns failure instead of SIGPIPE");
}

}  // namespace

int main() {
    wireContract();
    rejectsMalformedHeaders();
    respectsDestinationStride();
    readsFragmentedPacket();
    rejectsTruncatedPacket();
    boundsSilentPeer();
    cancellationReleasesReader();
    boundsStalledWriter();
    closedPeerCannotSignalProcess();
    std::puts("PASS: 9 camera protocol, stride and socket lifecycle tests");
    return EXIT_SUCCESS;
}
