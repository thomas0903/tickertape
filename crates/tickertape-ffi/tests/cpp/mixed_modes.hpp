// Shared declarations of the mixed-mode link test (tests/c_api.rs -> cpp_binding_links_units_built_in_different_modes).
// mixed_modes_unit.cpp is compiled once per mode (with and without exceptions, with and without debug checks) and
// the objects are linked into one program in both orders; mixed_modes_main.cpp runs one unit's behaviour per call.
#ifndef TICKERTAPE_MIXED_MODES_HPP
#define TICKERTAPE_MIXED_MODES_HPP

#include <cstddef>
#include <cstdint>

// A fill function type shared by every unit, so that Writer::append_with<SharedFill> is instantiated in each unit;
// only the inline namespace (exception mode) or the abi_tag (debug-check mode) tells the copies apart. Its call
// operator is defined once, in mixed_modes_main.cpp (built with exceptions), and throws when `throw_now` is set.
struct SharedFill {
    bool throw_now;
    void operator()(std::uint8_t *buf, std::size_t len) const;
};

// Defined in mixed_modes_main.cpp, which replaces operator new: the `n`th allocation from now throws std::bad_alloc
// (0: none). Returns whether the replacement is in use (false under a tool that replaces operator new itself).
bool tt_fail_nth_allocation(int n);

// Each unit defines these with its mode in the name: exc_ or noexc_, then dbg_ or nodbg_.
#define TT_MIXED_DECLARE(prefix)                                                                           \
    int prefix##value_on_error();          /* result<T>::value() on an error: 1 if it threw */             \
    int prefix##fill(const char *dir, bool throw_now); /* append_with: 1 committed, 2 rethrown, 0 other */ \
    int prefix##index_out_of_range();      /* RecordBatch::operator[] past size(): returns if unchecked */ \
    int prefix##list_segments(const char *dir); /* see mixed_modes_unit.cpp */

TT_MIXED_DECLARE(exc_dbg_)
TT_MIXED_DECLARE(exc_nodbg_)
TT_MIXED_DECLARE(noexc_nodbg_)

#endif
