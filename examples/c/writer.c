/*
 * Record events into a tickertape journal through the C ABI.
 *
 *   writer <dir> [count] [interval_ms]
 *
 * Appends `count` records (default 10), one every `interval_ms` milliseconds (default 0), checkpointing every 100
 * records and at the end. A record is visible to readers as soon as it is appended, and durable after the next
 * checkpoint.
 */
#define _POSIX_C_SOURCE 200809L

#include "tickertape.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static int fail(const char *what, tickertape_status status) {
    fprintf(stderr, "%s: %s: %s\n", what, tickertape_status_name(status), tickertape_last_error_message());
    return 1;
}

int main(int argc, char **argv) {
    tickertape_writer *writer = NULL;
    tickertape_status status;
    unsigned long count, interval_ms, i;
    if (argc < 2 || argc > 4) {
        fprintf(stderr, "usage: %s <dir> [count] [interval_ms]\n", argv[0]);
        return 2;
    }
    count = argc > 2 ? strtoul(argv[2], NULL, 10) : 10;
    interval_ms = argc > 3 ? strtoul(argv[3], NULL, 10) : 0;
    {
        /* Before 1.0 the ABI is unstable and the library must be exactly the header's version; from 1.0 it must
         * have the same ABI version and be at least as new (see TICKERTAPE_ABI_VERSION in the header). */
        uint32_t lib = tickertape_version_number();
        if (tickertape_abi_version() != TICKERTAPE_ABI_VERSION ||
            (TICKERTAPE_ABI_VERSION == 0 ? lib != TICKERTAPE_VERSION_NUMBER : lib < TICKERTAPE_VERSION_NUMBER)) {
            fprintf(stderr, "library %s does not match header %d.%d.%d\n", tickertape_version(),
                    TICKERTAPE_VERSION_MAJOR, TICKERTAPE_VERSION_MINOR, TICKERTAPE_VERSION_PATCH);
            return 1;
        }
    }

    status = tickertape_writer_open(argv[1], NULL, &writer); /* NULL options: defaults */
    if (status != TICKERTAPE_OK) return fail("open", status);

    for (i = 0; i < count; i++) {
        char payload[64];
        uint64_t seq;
        int len = snprintf(payload, sizeof payload, "tick %lu price %lu.%02lu", i, 100 + i / 100, i % 100);
        status = tickertape_writer_append(writer, 1 /* schema id */, 0 /* venue time: not set */,
                                          tickertape_realtime_ns(), (const uint8_t *)payload, (size_t)len, &seq);
        if (status != TICKERTAPE_OK) {
            tickertape_writer_close(writer);
            return fail("append", status);
        }
        printf("appended %llu\n", (unsigned long long)seq);
        fflush(stdout);
        if ((i + 1) % 100 == 0 && (status = tickertape_writer_checkpoint(writer)) != TICKERTAPE_OK) {
            tickertape_writer_close(writer);
            return fail("checkpoint", status);
        }
        if (interval_ms > 0) {
            struct timespec pause;
            pause.tv_sec = (time_t)(interval_ms / 1000);
            pause.tv_nsec = (long)(interval_ms % 1000) * 1000000L;
            nanosleep(&pause, NULL);
        }
    }

    status = tickertape_writer_checkpoint(writer); /* make everything durable before exiting */
    if (status != TICKERTAPE_OK) {
        tickertape_writer_close(writer);
        return fail("checkpoint", status);
    }
    if ((status = tickertape_writer_close(writer)) != TICKERTAPE_OK) return fail("close", status);
    return 0;
}
