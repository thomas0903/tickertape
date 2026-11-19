// The driver of the mixed-mode link test (see mixed_modes.hpp), built with exceptions. It does not include
// tickertape.hpp, so every copy of the header's inline functions comes from the units. One behaviour per run:
//
//   exceptions pair (TT_MIXED_PAIR_EXCEPTIONS): exc-value, noexc-value, exc-fill <dir>, noexc-fill <dir>
//   debug-check pair (TT_MIXED_PAIR_DEBUG_CHECKS): dbg-index, nodbg-index
//
// It prints what happened; a run that should abort aborts inside the unit.
#include "mixed_modes.hpp"

#include <cstdio>
#include <stdexcept>
#include <string>

void SharedFill::operator()(std::uint8_t *buf, std::size_t len) const {
    if (throw_now) throw std::runtime_error("the fill threw");
    if (len > 0) buf[0] = 1;
}

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
