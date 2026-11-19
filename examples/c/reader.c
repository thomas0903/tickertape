/*
 * Read a tickertape journal through the C ABI.
 *
 *   reader <dir> replay [from_seq]     print every committed record from `from_seq` (default: the oldest), then exit
 *   reader <dir> follow [count]        follow the journal live, from another process than the writer, and exit
 *                                      after `count` records (default: never)
 *
 * Set TICKERTAPE_DURABLE_ONLY=1 to read only records covered by a checkpoint (durable) instead of every visible one.
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

static void print_record(const tickertape_record *r) {
    /* `r->payload` is borrowed from the reader: valid only until the next call on it. */
    printf("%llu schema=%u local_ts_ns=%llu payload=%.*s\n", (unsigned long long)r->seq, r->schema_id,
           (unsigned long long)r->local_ts_ns, (int)r->payload_len, (const char *)r->payload);
    fflush(stdout);
}

/* Opens a reader, waiting until the writer has created the journal. */
static tickertape_status open_reader(const char *dir, const tickertape_reader_options *options,
                                     tickertape_reader **reader, int wait) {
    for (;;) {
        tickertape_status status = tickertape_reader_open(dir, options, reader);
        if (!wait || (status != TICKERTAPE_E_NOT_FOUND && status != TICKERTAPE_E_IO)) return status;
        {
            struct timespec pause = {0, 10000000L}; /* 10 ms */
            nanosleep(&pause, NULL);
        }
    }
}

int main(int argc, char **argv) {
    tickertape_reader_options *options;
    tickertape_reader *reader = NULL;
    tickertape_record record;
    tickertape_status status;
    const char *durable = getenv("TICKERTAPE_DURABLE_ONLY");
    int follow;
    unsigned long long limit = 0, seen = 0;

    if (argc < 3 || argc > 4 || (strcmp(argv[2], "replay") != 0 && strcmp(argv[2], "follow") != 0)) {
        fprintf(stderr, "usage: %s <dir> replay [from_seq] | follow [count]\n", argv[0]);
        return 2;
    }
    follow = strcmp(argv[2], "follow") == 0;

    options = tickertape_reader_options_new();
    if (options == NULL) return 1;
    tickertape_reader_options_set_durable_only(options, durable != NULL && strcmp(durable, "1") == 0);
    status = open_reader(argv[1], options, &reader, follow);
    tickertape_reader_options_free(options); /* the reader keeps a copy */
    if (status != TICKERTAPE_OK) return fail("open", status);

    if (!follow && argc == 4 && (status = tickertape_reader_seek(reader, strtoull(argv[3], NULL, 10))) != 0) {
        tickertape_reader_close(reader);
        return fail("seek", status);
    }
    if (follow && argc == 4) limit = strtoull(argv[3], NULL, 10);

    for (;;) {
        /* Replay: don't wait, stop at the end. Follow: wait (with wake-ups from the writer) for each record. */
        status = tickertape_reader_next(reader, follow ? TICKERTAPE_WAIT_FOREVER : TICKERTAPE_NO_WAIT, &record);
        if (status == TICKERTAPE_NONE) break;
        if (status == TICKERTAPE_E_DISCARDED) {
            /* Recovery or retention replaced the segment under us: continue on the current chain. */
            uint64_t position = 0;
            tickertape_reader_position(reader, &position);
            if ((status = tickertape_reader_seek(reader, position)) == TICKERTAPE_OK) continue;
        }
        if (status != TICKERTAPE_OK) {
            tickertape_reader_close(reader);
            return fail("next", status);
        }
        print_record(&record);
        if (++seen == limit) break;
    }
    tickertape_reader_close(reader);
    return 0;
}
