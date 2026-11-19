// One translation unit of the mixed-mode link test (see mixed_modes.hpp). tests/c_api.rs compiles this file in
// several modes (with and without -fno-exceptions, with TICKERTAPE_DEBUG_CHECKS 0 and 1) and links two of them into
// one program. Each copy must keep its own mode's behaviour whichever object the linker sees first.
#include "tickertape.hpp"

#include "mixed_modes.hpp"

#if TICKERTAPE_HPP_EXCEPTIONS && TICKERTAPE_DEBUG_CHECKS
#define TT_UNIT(name) exc_dbg_##name
#elif TICKERTAPE_HPP_EXCEPTIONS
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
#if TICKERTAPE_HPP_EXCEPTIONS
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
#if TICKERTAPE_HPP_EXCEPTIONS
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
