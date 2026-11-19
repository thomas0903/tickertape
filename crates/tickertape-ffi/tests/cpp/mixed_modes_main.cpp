// The driver of the mixed-mode link test (see mixed_modes.hpp), built with exceptions. It does not include
// tickertape.hpp, so every copy of the header's inline functions comes from the units. One behaviour per run:
//
//   exceptions pair (TT_MIXED_PAIR_EXCEPTIONS): exc-value, noexc-value, exc-fill <dir>, noexc-fill <dir>,
//                                               exc-alloc <dir>
//   debug-check pair (TT_MIXED_PAIR_DEBUG_CHECKS): dbg-index, nodbg-index
//
// It prints what happened; a run that should abort aborts inside the unit.
#include "mixed_modes.hpp"

#include <cstdio>
#include <cstdlib>
#include <new>
#include <stdexcept>
#include <string>

void SharedFill::operator()(std::uint8_t *buf, std::size_t len) const {
    if (throw_now) throw std::runtime_error("the fill threw");
    if (len > 0) buf[0] = 1;
}

static int fail_at = 0;      // the allocation (counted from tt_fail_nth_allocation) that throws; 0: none
static int allocations = 0;  // since tt_fail_nth_allocation
static bool replaced_single = false;
static bool replaced_array = false;

bool tt_fail_nth_allocation(int n) {
    // The probes run disarmed, so that neither is the allocation that fails; the count starts after them. Both forms
    // are replaced and probed: the binding allocates arrays with new[], and under AddressSanitizer or Valgrind the
    // tool's own operator new[] does not call the operator new below.
    fail_at = 0;
    replaced_single = false;
    replaced_array = false;
    ::operator delete(::operator new(1));      // never elided: tells whether these replacements are the ones in use
    ::operator delete[](::operator new[](1));
    allocations = 0;
    fail_at = n;
    return replaced_single && replaced_array;
}

static void *allocate(std::size_t size) {
    if (fail_at > 0 && ++allocations == fail_at) {
        fail_at = 0;
        throw std::bad_alloc();
    }
    if (void *p = std::malloc(size ? size : 1)) return p;
    throw std::bad_alloc();
}
void *operator new(std::size_t size) {
    replaced_single = true;
    return allocate(size);
}
void *operator new[](std::size_t size) {
    replaced_array = true;
    return allocate(size);
}
// Not inlined: GCC's -Wmismatched-new-delete would otherwise see free() applied to the result of operator new once
// the replacement operator delete is inlined into a caller.
__attribute__((noinline)) static void release(void *p) noexcept { std::free(p); }
void operator delete(void *p) noexcept { release(p); }
void operator delete(void *p, std::size_t) noexcept { release(p); }
void operator delete[](void *p) noexcept { release(p); }
void operator delete[](void *p, std::size_t) noexcept { release(p); }

int main(int argc, char **argv) {
    const std::string what = argc > 1 ? argv[1] : "";
#if defined(TT_MIXED_PAIR_EXCEPTIONS)
    const char *dir = argc > 2 ? argv[2] : "";
    if (what == "exc-value") {
        std::printf("%s\n", exc_nodbg_value_on_error() == 1 ? "threw" : "did not throw");
        return 0;
    }
    if (what == "noexc-value") {
        std::printf("returned %d\n", noexc_nodbg_value_on_error());  // must abort before this
        return 0;
    }
    if (what == "exc-fill") {
        const int r = exc_nodbg_fill(dir, true);
        std::printf("%s\n", r == 2 ? "rethrown" : r == 1 ? "committed" : "failed");
        return 0;
    }
    if (what == "noexc-fill") {
        const int r = noexc_nodbg_fill(dir, false);
        std::printf("%s\n", r == 1 ? "committed" : "failed");
        return 0;
    }
    if (what == "exc-alloc") {
        // The no-exceptions unit's copy of list_segments is in the program too (it is called here once).
        if (noexc_nodbg_list_segments(dir) < 1) return 3;
        std::printf("caught %d\n", exc_nodbg_list_segments(dir));
        return 0;
    }
#elif defined(TT_MIXED_PAIR_DEBUG_CHECKS)
    if (what == "dbg-index") {
        std::printf("returned %d\n", exc_dbg_index_out_of_range());  // must abort before this
        return 0;
    }
    if (what == "nodbg-index") {
        std::printf("unchecked %d\n", exc_nodbg_index_out_of_range());
        return 0;
    }
#else
#error "define TT_MIXED_PAIR_EXCEPTIONS or TT_MIXED_PAIR_DEBUG_CHECKS"
#endif
    std::fprintf(stderr, "unknown command %s\n", what.c_str());
    return 2;
}
