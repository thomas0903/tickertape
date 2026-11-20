/*
 * Record, follow live and replay a tickertape journal from Java. A single source file: run it with the java launcher,
 * no build step needed (JDK 25 or newer).
 *
 *   java --enable-native-access=ALL-UNNAMED -cp <tickertape jar> [-Dtickertape.library.path=<libtickertape>] \
 *       Example.java record <dir> [count] [interval_ms]    append records, checkpointing every 100 and at the end
 *   ... Example.java follow <dir> [count]                    follow live, from another process, and exit after count
 *   ... Example.java replay <dir> [from_seq]                 print every committed record from from_seq, then exit
 *
 * Set TICKERTAPE_DURABLE_ONLY=1 to follow or replay only records covered by a checkpoint (durable) instead of every
 * visible one. See bindings/java/README.md for the full commands.
 */

import io.github.thomas0903.tickertape.DiscardedException;
import io.github.thomas0903.tickertape.Journal;
import io.github.thomas0903.tickertape.JournalIOException;
import io.github.thomas0903.tickertape.JournalNotFoundException;
import io.github.thomas0903.tickertape.Reader;
import io.github.thomas0903.tickertape.ReaderOptions;
import io.github.thomas0903.tickertape.RecordView;
import io.github.thomas0903.tickertape.SeqUnavailableException;
import io.github.thomas0903.tickertape.Tickertape;
import io.github.thomas0903.tickertape.Writer;
import java.nio.charset.StandardCharsets;
import java.nio.file.Path;
import java.util.concurrent.TimeUnit;

public class Example {
    private static final int ENOENT = 2;

    public static void main(String[] args) throws Exception {
        if (args.length < 2) {
            System.err.println("usage: Example record <dir> [count] [interval_ms] | follow <dir> [count] | replay <dir> [from_seq]");
            System.exit(2);
        }
        Path dir = Path.of(args[1]);
        switch (args[0]) {
            case "record" -> record(dir, arg(args, 2, 10), arg(args, 3, 0));
            case "follow" -> follow(dir, arg(args, 2, Long.MAX_VALUE));
            case "replay" -> replay(dir, arg(args, 2, -1));
            default -> {
                System.err.println("unknown command " + args[0]);
                System.exit(2);
            }
        }
    }

    private static long arg(String[] args, int i, long otherwise) {
        return args.length > i ? Long.parseLong(args[i]) : otherwise;
    }

    /** Appends {@code count} records: visible to readers at once, durable at each checkpoint. */
    static void record(Path dir, long count, long intervalMs) throws InterruptedException {
        try (Writer writer = Writer.open(dir)) {
            for (long i = 0; i < count; i++) {
                String text = "tick " + i + " price " + (100 + i / 100) + "." + String.format("%02d", i % 100);
                byte[] payload = text.getBytes(StandardCharsets.UTF_8);
                long seq;
                if (i % 2 == 0) {
                    seq = writer.append(1, 0, Tickertape.realtimeNs(), payload);
                } else {
                    // Zero-copy: the payload is written straight into the journal's mapped segment.
                    seq = writer.appendWith(1, 0, Tickertape.realtimeNs(), payload.length, buf -> buf.put(0, payload));
                }
                System.out.println("appended " + seq);
                if ((i + 1) % 100 == 0) {
                    writer.checkpoint();
                }
                if (intervalMs > 0) {
                    Thread.sleep(intervalMs);
                }
            }
            writer.checkpoint(); // make everything durable before exiting
        }
    }

    private static ReaderOptions options() {
        return ReaderOptions.defaults().durableOnly("1".equals(System.getenv("TICKERTAPE_DURABLE_ONLY")));
    }

    private static void print(RecordView r) {
        // The view is borrowed: valid until the next call on the reader.
        System.out.println(r.seq() + " schema=" + Integer.toUnsignedString(r.schemaId()) + " local_ts_ns="
                + Long.toUnsignedString(r.localTsNs()) + " payload=" + new String(r.payloadToArray(), StandardCharsets.UTF_8));
    }

    /** Follows the journal live, starting before the writer if need be, and exits after {@code count} records. */
    static void follow(Path dir, long count) throws InterruptedException {
        try (Reader reader = openWhenReady(dir)) {
            long seen = 0;
            while (seen < count) {
                try {
                    RecordView r = reader.next(100, TimeUnit.MILLISECONDS);
                    if (r != null) {
                        print(r);
                        seen++;
                    }
                } catch (DiscardedException e) {
                    // Recovery or retention replaced the segment at the position: continue on the current chain.
                    try {
                        reader.seek(reader.position());
                    } catch (SeqUnavailableException gone) {
                        reader.seek(Journal.listSegments(dir).get(0).baseSeq());
                    }
                }
            }
        }
    }

    /** Before the writer starts the directory does not exist (ENOENT); while it creates the first segment it is empty. */
    private static Reader openWhenReady(Path dir) throws InterruptedException {
        for (;;) {
            try {
                return Reader.open(dir, options());
            } catch (JournalNotFoundException e) {
                Thread.sleep(10);
            } catch (JournalIOException e) {
                if (e.osErrorCode() != ENOENT) {
                    throw e;
                }
                Thread.sleep(10);
            }
        }
    }

    /** Prints every committed record from {@code from} (the oldest if negative), then exits. */
    static void replay(Path dir, long from) {
        try (Reader reader = Reader.open(dir, options())) {
            if (from >= 0) {
                reader.seek(from);
            }
            for (RecordView r; (r = reader.tryNext()) != null; ) {
                print(r);
            }
        }
    }
}
