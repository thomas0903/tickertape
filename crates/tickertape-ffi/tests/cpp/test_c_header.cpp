// The C header (include/tickertape.h) on its own from C++, compiled by tests/c_api.rs as C++11 and C++20: the C ABI
// promises a header that C++11 code can include (ADR 0028), independently of the C++17 binding.
#include "tickertape.h"

#include <cstdio>

int main() {
    const uint32_t lib = tickertape_version_number();
    const bool compatible = tickertape_abi_version() == static_cast<uint32_t>(TICKERTAPE_ABI_VERSION) &&
                            lib == static_cast<uint32_t>(TICKERTAPE_VERSION_NUMBER);
    tickertape_reader *reader = nullptr;
    const tickertape_status status = tickertape_reader_open("/nonexistent/tickertape", nullptr, &reader);
    if (!compatible || status != TICKERTAPE_E_IO || reader != nullptr) {
        std::fprintf(stderr, "unexpected: compatible=%d status=%s\n", compatible ? 1 : 0,
                     tickertape_status_name(status));
        return 1;
    }
    std::printf("ok\n");
    return 0;
}
