/* SPDX-License-Identifier: GPL-3.0-or-later */
#include <array>
#include <cassert>
#include <cstdint>
#include <cstring>
#include <fcntl.h>
#include <functional>
#include <mutex>
#include <sys/eventfd.h>
#include <sys/socket.h>
#include <thread>
#include <unistd.h>
#include <utility>
#include <vector>

// Only the Android RAII FD type needs a host substitute. The transport below
// is extracted from DroidloomLayers.cpp after applying the real patch chain.
namespace base {
class unique_fd {
    int fd = -1;
public:
    unique_fd() = default;
    explicit unique_fd(int value) : fd(value) {}
    ~unique_fd() { if (fd >= 0) close(fd); }
    unique_fd(const unique_fd&) = delete;
    unique_fd& operator=(const unique_fd&) = delete;
    unique_fd(unique_fd&& other) noexcept : fd(std::exchange(other.fd, -1)) {}
    unique_fd& operator=(unique_fd&& other) noexcept {
        if (fd >= 0) close(fd);
        fd = std::exchange(other.fd, -1);
        return *this;
    }
};
}
#include "DroidloomLayerTransport.inc"

void replyWords(int socket, uint32_t opcode, const std::vector<uint32_t>& words) {
    std::vector<uint8_t> bytes;
    for (auto word : words) put32(bytes, word);
    assert(send(socket, opcode, bytes));
}

void configurationTests() {
    ConfigCache cache;
    int repaints = 0;
    const auto repaint = [&] {
        // Callback may read the snapshot; the update must release its lock.
        assert(cache.snapshot()[2] == 800);
        ++repaints;
    };
    assert(configurationEvent(cache, {2, 0, 15, 10, 800, 600}, repaint));
    assert(repaints == 1);
    assert(configurationEvent(cache, {2, 0, 15, 10, 800, 600}, repaint));
    assert(configurationEvent(cache, {1, 0, 15, 9, 800, 600}, repaint));
    assert(repaints == 1);
    assert(!configurationEvent(cache, {2, 0, 0, 10, 800, 600}, repaint));
    assert(!configurationEvent(cache, {0, 0, 15, 10, 800, 600}, repaint));
    assert(!configurationEvent(cache, {3, 0, 15, 10}, repaint));
    assert(repaints == 1);
    // Visibility can change while Android's configure serial stays the same.
    assert(configurationEvent(cache, {3, 0, 0, 10, 800, 600}, repaint));
    assert(repaints == 2 && cache.snapshot()[0] == 0);

    int sockets[2];
    assert(socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0, sockets) == 0);
    ConfigCache current;
    bool subscribed = false;
    std::array<uint32_t, 4> config;
    std::thread host([&] {
        std::vector<uint32_t> request;
        assert(receive(sockets[1], 1, &request) && request.empty());
        replyWords(sockets[1], 0x8002, {15, 10, 800, 600});
        assert(receive(sockets[1], 8, &request) && request.empty());
        // Event receiver wins the race with the initial reply on the other FD.
        assert(current.update({2, 0, 15, 11, 1024, 768}));
        replyWords(sockets[1], 0x8005, {1, 0, 15, 10, 800, 600});
    });
    assert(!configuration(sockets[0], current, subscribed, 800, 600, &config));
    host.join();
    assert(subscribed && config[1] == 11);
    assert(fcntl(sockets[0], F_SETFL, O_NONBLOCK) == 0);
    for (int frame = 0; frame != 1000; ++frame)
        assert(configuration(sockets[0], current, subscribed, 1024, 768, &config));
    char byte;
    assert(recv(sockets[1], &byte, 1, MSG_DONTWAIT) == -1 && errno == EAGAIN);
    assert(current.update({3, 0, 0, 11, 1024, 768}));
    assert(!configuration(sockets[0], current, subscribed, 1024, 768, &config));
    assert(current.update({4, 0, 15, 11, 1024, 768}));
    assert(configuration(sockets[0], current, subscribed, 1024, 768, &config));
    assert(recv(sockets[1], &byte, 1, MSG_DONTWAIT) == -1 && errno == EAGAIN);
    // A host without CONFIG_EVENTS still receives one Config per call.
    assert(fcntl(sockets[0], F_SETFL, 0) == 0);
    subscribed = false;
    std::thread legacy([&] {
        for (uint32_t serial = 12; serial != 14; ++serial) {
            std::vector<uint32_t> request;
            assert(receive(sockets[1], 1, &request) && request.empty());
            replyWords(sockets[1], 0x8002, {7, serial, 800, 600});
        }
    });
    for (uint32_t serial = 12; serial != 14; ++serial) {
        assert(configuration(sockets[0], current, subscribed, 800, 600, &config));
        assert(!subscribed && config[1] == serial);
    }
    legacy.join();
    close(sockets[0]);
    close(sockets[1]);
}

int main() {
    configurationTests();
    int sockets[2];
    assert(socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0, sockets) == 0);
    // Nonblocking makes any accidental stage receive fail immediately. Queue
    // the entire 64-layer scene before the mock compositor sends any reply.
    assert(fcntl(sockets[0], F_SETFL, O_NONBLOCK) == 0);
    int event = eventfd(1, EFD_CLOEXEC | EFD_NONBLOCK);
    assert(event >= 0);
    for (uint64_t layer = 1; layer <= 64; ++layer) {
        std::vector<uint8_t> payload;
        put64(payload, layer);
        payload.resize(72);
        assert(stage(sockets[0], true, payload, {event}));
    }
    close(event);
    std::vector<uint8_t> commit;
    for (uint32_t value : {1, 800, 600, 64}) put32(commit, value);
    assert(send(sockets[0], 5, commit));
    for (uint64_t layer = 1; layer <= 64; ++layer) {
        std::array<uint8_t, 128> bytes{};
        iovec iov{bytes.data(), bytes.size()};
        alignas(cmsghdr) std::array<uint8_t, CMSG_SPACE(sizeof(int))> control{};
        msghdr msg{};
        msg.msg_iov = &iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.data();
        msg.msg_controllen = control.size();
        assert(recvmsg(sockets[1], &msg, MSG_CMSG_CLOEXEC) == 88);
        assert(msg.msg_flags == 0 || msg.msg_flags == MSG_CMSG_CLOEXEC);
        assert(u32(bytes.data() + 4) == 7 && u32(bytes.data() + 12) == 1);
        assert(u32(bytes.data() + 16) == layer);
        auto* header = CMSG_FIRSTHDR(&msg);
        assert(header && header->cmsg_type == SCM_RIGHTS);
        int imported;
        std::memcpy(&imported, CMSG_DATA(header), sizeof(imported));
        assert(fcntl(imported, F_GETFD) & FD_CLOEXEC);
        close(imported);
    }
    std::vector<uint32_t> words;
    assert(receive(sockets[1], 5, &words) && words.size() == 4 && words[3] == 64);
    // Both rejection and acceptance are consumed only at the explicit barrier.
    std::vector<uint8_t> rejected;
    put32(rejected, static_cast<uint32_t>(-95));
    assert(send(sockets[1], 0x8001, rejected));
    assert(receive(sockets[0], 0x8001, &words) && words[0] == static_cast<uint32_t>(-95));
    assert(fcntl(sockets[0], F_SETFL, 0) == 0);
    for (bool accepted : {true, false}) {
        std::thread host([&] {
            std::vector<uint32_t> request;
            assert(receive(sockets[1], 4, &request));
            std::vector<uint8_t> reply;
            put32(reply, accepted ? 0 : static_cast<uint32_t>(-95));
            assert(send(sockets[1], 0x8001, reply));
        });
        assert(stage(sockets[0], false, std::vector<uint8_t>(72), {}) == accepted);
        host.join();
    }
    close(sockets[1]);
    assert(!stage(sockets[0], true, std::vector<uint8_t>(72), {}));
    close(sockets[0]);
}
