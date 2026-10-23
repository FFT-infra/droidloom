/* SPDX-License-Identifier: GPL-3.0-or-later */
#include "DroidloomReleaseQueue.h"
#include <atomic>
#include <cassert>
#include <thread>

// An identity-bearing unsignalled fence: callback readiness must not imply
// producer reuse. No graphics device or running Android session is needed.
struct Fence { int id; std::shared_ptr<std::atomic<bool>> signalled; };
using Queue = android::DroidloomReleaseQueue<Fence>;

int main() {
    Queue queue;
    const Queue::Key a{10, 1}, b{20, 1};
    int surfaceA, surfaceB;
    auto stalled = queue.track(a);
    auto active = queue.track(b);
    std::vector<int> delivered;
    queue.defer({a}, {&surfaceA}, [&](auto) { delivered.push_back(1); });
    queue.defer({}, {&surfaceA}, [&](auto) { delivered.push_back(2); });
    queue.defer({}, {}, [&](auto) { delivered.push_back(3); }); // ON_COMMIT
    auto signal = std::make_shared<std::atomic<bool>>(false);
    queue.defer({b}, {&surfaceB}, [&](auto fences) {
        assert(fences.at(b).at(0).id == 42);
        assert(!fences.at(b).at(0).signalled->load());
        delivered.push_back(4);
    });
    assert(queue.publish(active, Fence{42, signal}));
    assert((delivered == std::vector<int>{3, 4}));
    assert(!queue.publish(active, Fence{43, signal})); // duplicate fence
    signal->store(true);
    assert(queue.publish(active, Fence{42, signal}, true));
    assert(!queue.publish(active, Fence{42, signal}, true));
    // A disconnected/unresolved read stays pending while another surface advances.
    assert((delivered == std::vector<int>{3, 4}));
    assert(queue.publish(stalled, Fence{7, signal}, true)); // proven rejection
    assert((delivered == std::vector<int>{3, 4, 1, 2}));

    // One producer frame read through two outputs must retain both fences.
    auto first = queue.track(a), second = queue.track(a);
    bool merged = false;
    queue.publish(first, Fence{50, signal});
    queue.defer({a, a}, {}, [&](auto fences) {
        assert(fences.size() == 1);
        assert(fences.at(a).size() == 2);
        assert(fences.at(a)[0].id == 50 && fences.at(a)[1].id == 51);
        merged = true;
    });
    assert(!merged);
    queue.publish(second, Fence{51, signal});
    assert(merged);

    // Publication racing subscription must deliver exactly once. A different
    // frame of the same allocation must not satisfy the pending dependency.
    for (uint64_t frame = 2; frame < 130; ++frame) {
        Queue q;
        Queue::Key key{10, frame};
        auto r = q.track(key);
        auto other = q.track({10, frame + 1});
        std::atomic<int> calls{0};
        std::thread publisher([&] { q.publish(r, Fence{99, signal}); });
        q.defer({key}, {&surfaceA}, [&](auto fences) {
            assert(fences.at(key)[0].id == 99);
            ++calls;
        });
        publisher.join();
        assert(calls == 1);
        bool wrongFrame = false;
        q.defer({{10, frame + 1}}, {}, [&](auto) { wrongFrame = true; });
        assert(!wrongFrame);
        q.publish(other, Fence{100, signal}, true);
        assert(wrongFrame);
    }

    // A completion touching two surfaces waits for both and holds both lanes;
    // a third surface of the same app is never delayed by them.
    {
        Queue q;
        int surfaceC;
        const Queue::Key held{30, 1};
        auto heldRead = q.track(held);
        std::vector<int> order;
        q.defer({held}, {&surfaceA}, [&](auto) { order.push_back(1); });
        q.defer({}, {&surfaceA, &surfaceB}, [&](auto) { order.push_back(2); });
        q.defer({}, {&surfaceB}, [&](auto) { order.push_back(3); });
        q.defer({}, {&surfaceC}, [&](auto) { order.push_back(4); });
        assert((order == std::vector<int>{4}));
        q.publish(heldRead, Fence{60, signal}, true);
        assert((order == std::vector<int>{4, 1, 2, 3}));
    }
}
