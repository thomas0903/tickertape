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

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static int fail(const char *what, tickertape_status status) {
    fprintf(stderr, "%s: %s: %s\n", what, tickertape_status_name(status), tickertape_last_error_message());
    return 1;
}

static void print_record(const tickertape_record *r) {
    /* `r->payload` is borrowed from the reader: valid only until the next call on it. It is NULL for an empty
     * payload, which printf("%.*s") must not receive. */
    printf("%llu schema=%u local_ts_ns=%llu payload=%.*s\n", (unsigned long long)r->seq, r->schema_id,
           (unsigned long long)r->local_ts_ns, (int)r->payload_len,
           r->payload_len > 0 ? (const char *)r->payload : "");
    fflush(stdout);
}

/* Opens a reader, waiting until the writer has created the journal: before the writer starts, the directory does
 * not exist (TICKERTAPE_E_IO with ENOENT); while it creates the first segment, the directory has none
 * (TICKERTAPE_E_NOT_FOUND). Any other error is real. */
static tickertape_status open_reader(const char *dir, const tickertape_reader_options *options,
                                     tickertape_reader **reader, int wait) {
    for (;;) {
        tickertape_status status = tickertape_reader_open(dir, options, reader);
        int not_yet = status == TICKERTAPE_E_NOT_FOUND ||
                      (status == TICKERTAPE_E_IO && tickertape_last_error_os_code() == ENOENT);
        if (!wait || !not_yet) return status;
        {
            struct timespec pause = {0, 10000000L}; /* 10 ms */
            nanosleep(&pause, NULL);
        }
    }
}

/* Stores the sequence number of the oldest record still in the journal (the oldest segment's base) in *seq. */
static tickertape_status oldest_seq(const char *dir, uint64_t *seq) {
    for (;;) {
        size_t count = 0;
        tickertape_segment_info *segments;
        tickertape_status status = tickertape_list_segments(dir, NULL, 0, &count);
        if (status != TICKERTAPE_OK) return status;
        segments = malloc(count * sizeof *segments);
        if (segments == NULL) return TICKERTAPE_E_IO;
        status = tickertape_list_segments(dir, segments, count, &count);
        if (status == TICKERTAPE_OK) *seq = segments[0].base_seq;
        free(segments);
        if (status != TICKERTAPE_E_BUFFER_TOO_SMALL) return status; /* else a segment was added: retry */
    }
}

/* Continues after TICKERTAPE_E_DISCARDED: recovery or retention replaced or removed the reader's segment. Seeks to
 * the same position on the current chain; if retention removed it, skips to the oldest record left and says so. */
static tickertape_status recover_discarded(const char *dir, tickertape_reader *reader) {
    uint64_t position = 0, oldest = 0;
    tickertape_status status = tickertape_reader_position(reader, &position);
    if (status != TICKERTAPE_OK) return status;
    status = tickertape_reader_seek(reader, position);
    if (status != TICKERTAPE_E_SEQ_UNAVAILABLE) return status;
    if ((status = oldest_seq(dir, &oldest)) != TICKERTAPE_OK) return status;
    fprintf(stderr, "records %llu..%llu were removed by retention before they were read; continuing at %llu\n",
            (unsigned long long)position, (unsigned long long)oldest - 1, (unsigned long long)oldest);
    return tickertape_reader_seek(reader, oldest);
}

static int library_is_compatible(void) {
    uint32_t lib = tickertape_version_number();
    return tickertape_abi_version() == TICKERTAPE_ABI_VERSION &&
           (TICKERTAPE_ABI_VERSION == 0 ? lib == TICKERTAPE_VERSION_NUMBER : lib >= TICKERTAPE_VERSION_NUMBER);
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
    if (!library_is_compatible()) {
        fprintf(stderr, "library %s does not match header %d.%d.%d\n", tickertape_version(), TICKERTAPE_VERSION_MAJOR,
                TICKERTAPE_VERSION_MINOR, TICKERTAPE_VERSION_PATCH);
        return 1;
    }

    options = tickertape_reader_options_new(); /* never NULL: the library aborts if it cannot allocate */
    status = tickertape_reader_options_set_durable_only(options, durable != NULL && strcmp(durable, "1") == 0);
    if (status == TICKERTAPE_OK) status = open_reader(argv[1], options, &reader, follow);
    tickertape_reader_options_free(options); /* the reader keeps a copy */
    if (status != TICKERTAPE_OK) return fail("open", status);

    if (!follow && argc == 4 && (status = tickertape_reader_seek(reader, strtoull(argv[3], NULL, 10))) != 0) {
        tickertape_reader_close(reader);
        return fail("seek", status);
    }
    if (follow && argc == 4) limit = strtoull(argv[3], NULL, 10);

    for (;;) {
        /* Replay: don't wait, stop at the end. Follow: wait (with wake-ups from the writer) for each record, in
         * slices of one second so that a real program could check for a stop request between calls; a call with
         * TICKERTAPE_WAIT_FOREVER cannot be interrupted. */
        status = tickertape_reader_next(reader, follow ? 1000000000 : TICKERTAPE_NO_WAIT, &record);
        if (status == TICKERTAPE_NONE) {
            if (follow) continue;
            break;
        }
        if (status == TICKERTAPE_E_DISCARDED) {
            if ((status = recover_discarded(argv[1], reader)) == TICKERTAPE_OK) continue;
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
