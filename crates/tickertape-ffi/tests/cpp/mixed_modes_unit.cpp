// One translation unit of the mixed-mode link test (see mixed_modes.hpp). tests/c_api.rs compiles this file in
// several modes (with and without -fno-exceptions, with TICKERTAPE_DEBUG_CHECKS 0 and 1) and links two of them into
// one program. Each copy must keep its own mode's behaviour whichever object the linker sees first.
#include "tickertape.hpp"

#include "mixed_modes.hpp"

#include <new>
#include <vector>

#if defined(__cpp_exceptions) && TICKERTAPE_DEBUG_CHECKS
#define TT_UNIT(name) exc_dbg_##name
#elif defined(__cpp_exceptions)
#define TT_UNIT(name) exc_nodbg_##name
#elif !TICKERTAPE_DEBUG_CHECKS
#define TT_UNIT(name) noexc_nodbg_##name
#else
#error "the mixed-mode test has no unit without exceptions but with debug checks"
#endif

namespace tt = tickertape;

int TT_UNIT(value_on_error)() {
    tt::result<int> failed = tt::Error(tt::Status::busy, "mixed-mode test");
    const tt::result<void> failed_void = tt::Error(tt::Status::busy, "mixed-mode test");
#if defined(__cpp_exceptions)
    int thrown = 0;
    try {
        (void)failed.value();
    } catch (const tt::Exception &e) {
        thrown += e.status() == tt::Status::busy ? 1 : 0;
    }
    try {
        failed_void.value();
    } catch (const tt::Exception &) {
        thrown++;
    }
    try {
        (void)std::move(failed).value();
    } catch (const tt::Exception &) {
        thrown++;
    }
    return thrown == 3 ? 1 : 0;
#else
    (void)failed_void;
    return failed.value();  // prints the error and aborts
#endif
}

int TT_UNIT(fill)(const char *dir, bool throw_now) {
    tt::result<tt::Writer> opened = tt::Writer::open(dir);
    if (!opened) return 0;
    tt::Writer writer = std::move(opened).value();
    SharedFill fill{throw_now};
#if defined(__cpp_exceptions)
    try {
        return writer.append_with(1, 0, 0, 8, fill) ? 1 : 0;
    } catch (const std::runtime_error &) {
        return 2;
    }
#else
    return writer.append_with(1, 0, 0, 8, fill) ? 1 : 0;
#endif
}

int TT_UNIT(index_out_of_range)() {
    const tt::RecordBatch batch(4, 64);
    // size() is 0: with debug checks this aborts; without, it reads the zeroed (allocated) record 2.
    return static_cast<int>(batch[2].seq());
}

// list_segments is an ordinary inline function: it mentions no mode macro and carries no abi_tag, but it contains
// exception handling the compiler generates (it calls reserve() while another vector is live). Only the inline
// namespace keeps the copy built without exceptions out of the exceptions unit. Creates a journal in `dir`, then:
// with exceptions, makes each allocation inside list_segments fail in turn (1st, 2nd, ...) until a call succeeds,
// catching every std::bad_alloc here, and returns how many it caught (-1 for another outcome); without
// exceptions, returns the number of segments.
int TT_UNIT(list_segments)(const char *dir) {
    {
        tt::WriterOptions options;
        options.segment_size(65536).prefault(false);
        tt::result<tt::Writer> opened = tt::Writer::open(dir, options);
        if (!opened) return -1;
    }
#if defined(__cpp_exceptions)
    int caught = 0;
    for (int n = 1; n <= 32; n++) {
        try {
            if (!tt_fail_nth_allocation(n)) return -1;  // operator new is not the test's
            const tt::result<std::vector<tt::SegmentInfo>> segments = tt::list_segments(dir);
            tt_fail_nth_allocation(0);
            return segments && !segments->empty() ? caught : -1;
        } catch (const std::bad_alloc &) {
            caught++;
        }
    }
    return -1;
#else
    const tt::result<std::vector<tt::SegmentInfo>> segments = tt::list_segments(dir);
    return segments ? static_cast<int>(segments->size()) : -1;
#endif
}
