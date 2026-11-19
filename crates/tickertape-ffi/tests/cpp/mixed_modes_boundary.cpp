// Passing a binding type between units built in different exception modes must fail to link (ADR 0036 -> Mixing
// modes): tickertape::Writer is tickertape::v1::Writer with exceptions and tickertape::v1_noexcept::Writer without,
// so the same declaration names a different function in each mode. tests/c_api.rs builds this file twice, once with
// TT_BOUNDARY_DEFINE (the definition) and once without (the caller), and links the two: in different exception modes
// the link must fail; in the same mode (the control) it must succeed.
#include "tickertape.hpp"

bool tt_boundary_is_open(const tickertape::Writer &writer);

#if defined(TT_BOUNDARY_DEFINE)
bool tt_boundary_is_open(const tickertape::Writer &writer) { return writer.is_open(); }
#else
int main() {
    const tickertape::Writer writer;
    return tt_boundary_is_open(writer) ? 1 : 0;
}
#endif
