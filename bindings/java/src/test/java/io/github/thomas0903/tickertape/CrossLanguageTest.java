package io.github.thomas0903.tickertape;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.io.IOException;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.MessageDigest;
import java.util.ArrayList;
import java.util.HexFormat;
import java.util.List;
import java.util.Map;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

/**
 * The shared cross-language fixtures (ADR 0031): a journal written from Java reads back identically through the C ABI
 * test program, the Rust CLI and the Python decoder; journals written through C and by every released Rust writer
 * ({@code tests/compat/}) read back identically in Java.
 */
class CrossLanguageTest {
    @TempDir
    Path tmp;

    /** Writes the fixture of {@code crates/tickertape-ffi/tests/c_api.rs} with every Java append form. */
    static void writeJavaFixture(Path dir) {
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            AppendBatch batch = new AppendBatch();
            for (long seq = 0; seq < TestSupport.FIXTURE_RECORDS; seq++) {
                if (seq == TestSupport.FIXTURE_CHECKPOINT_AT) {
                    w.checkpoint();
                }
                byte[] payload = TestSupport.patternPayload(seq);
                int schema = TestSupport.patternSchema(seq);
                long venue = TestSupport.patternVenue(seq);
                long local = TestSupport.patternLocal(seq);
                long got;
                switch ((int) (seq % 4)) {
                    case 0 -> got = w.append(schema, venue, local, payload);
                    case 1 -> got = w.appendWith(schema, venue, local, payload.length, p -> p.put(0, payload));
                    case 2 -> got = w.append(schema, venue, local, ByteBuffer.wrap(payload));
                    default -> {
                        batch.clear();
                        got = w.appendBatch(batch.add(schema, venue, local, payload));
                    }
                }
                assertEquals(seq, got);
            }
        }
    }

    private static List<String> javaJsonl(Path dir, boolean durableOnly) {
        List<String> out = new ArrayList<>();
        try (Reader r = Reader.open(dir, ReaderOptions.defaults().durableOnly(durableOnly))) {
            long seq = 0;
            for (RecordView v; (v = r.tryNext()) != null; seq++) {
                TestSupport.checkPattern(v, seq);
                out.add(TestSupport.jsonl(v));
            }
        }
        return out;
    }

    @Test
    void javaWrittenJournalReadsBackThroughCRustAndPython() throws Exception {
        Path dir = tmp.resolve("from-java");
        writeJavaFixture(dir);
        assertTrue(Journal.listSegments(dir).size() >= 2, "the fixture must roll");
        List<String> all = javaJsonl(dir, false);
        List<String> durable = javaJsonl(dir, true);
        assertEquals(TestSupport.FIXTURE_RECORDS, all.size());
        assertEquals(TestSupport.FIXTURE_CHECKPOINT_AT, durable.size());

        // The C ABI test program reads it (all records, the checkpoint, durable-only reads).
        TestSupport.runOk(List.of(TestSupport.cTest().toString(), "read-fixture", dir.toString(),
                Long.toString(TestSupport.FIXTURE_RECORDS), Long.toString(TestSupport.FIXTURE_CHECKPOINT_AT)), 60);

        // The Rust CLI verifies it and exports exactly the same records.
        String cli = TestSupport.cli().toString();
        TestSupport.runOk(List.of(cli, "verify", dir.toString()), 60);
        assertEquals(all, TestSupport.lines(TestSupport.runOk(List.of(cli, "export", dir.toString(), "--format", "jsonl"), 60).stdout()));
        assertEquals(durable, TestSupport.lines(TestSupport.runOk(
                List.of(cli, "export", dir.toString(), "--format", "jsonl", "--durable-only"), 60).stdout()));

        // The dependency-free Python decoder agrees.
        String python = TestSupport.python();
        String decoder = TestSupport.REPO.resolve("python/tickertape_reader.py").toString();
        assertEquals(all, TestSupport.lines(TestSupport.runOk(List.of(python, "-I", decoder, dir.toString()), 60).stdout()));
        assertEquals(durable, TestSupport.lines(
                TestSupport.runOk(List.of(python, "-I", decoder, dir.toString(), "--durable-only"), 60).stdout()));
    }

    @Test
    void cWrittenJournalReadsBackInJava() throws Exception {
        Path dir = tmp.resolve("from-c");
        TestSupport.runOk(List.of(TestSupport.cTest().toString(), "write-fixture", dir.toString()), 60);
        for (boolean mapped : new boolean[] {false, true}) {
            for (boolean durableOnly : new boolean[] {false, true}) {
                ReaderOptions o = ReaderOptions.defaults().durableOnly(durableOnly);
                try (Reader r = mapped ? Reader.openMappedUnsafe(dir, o) : Reader.open(dir, o)) {
                    long n = durableOnly ? TestSupport.FIXTURE_CHECKPOINT_AT : TestSupport.FIXTURE_RECORDS;
                    for (long seq = 0; seq < n; seq++) {
                        TestSupport.checkPattern(r.tryNext(), seq);
                    }
                    assertNull(r.tryNext());
                }
            }
        }
        try (Reader r = Reader.open(dir)) {
            ReadBatch batch = new ReadBatch(100, 16 * 1024);
            long seq = 0;
            for (int n; (n = r.tryReadBatch(batch)) > 0; ) {
                for (int i = 0; i < n; i++) {
                    TestSupport.checkPattern(batch.get(i), seq++);
                }
            }
            assertEquals(TestSupport.FIXTURE_RECORDS, seq);
        }
        assertEquals(TestSupport.FIXTURE_CHECKPOINT_AT, Journal.readCheckpoint(dir).orElseThrow().nextSeq());
        assertEquals(TestSupport.FIXTURE_RECORDS, Journal.verify(dir).records());
    }

    @Test
    void theCReaderFollowsAJavaWriterLive() throws Exception {
        Path dir = tmp.resolve("live");
        Process follower = new ProcessBuilder(TestSupport.cReader().toString(), dir.toString(), "follow", "30")
                .redirectError(ProcessBuilder.Redirect.INHERIT).start();
        Thread.sleep(100); // it starts before the journal exists and waits for the writer
        try (Writer w = Writer.open(dir)) {
            for (int i = 0; i < 30; i++) {
                w.append(1, 0, 1000 + i, ("tick " + i).getBytes(StandardCharsets.UTF_8));
                Thread.sleep(2);
            }
        }
        assertTrue(follower.waitFor(60, TimeUnit.SECONDS), "the C follower did not finish");
        List<String> lines = TestSupport.lines(new String(follower.getInputStream().readAllBytes(), StandardCharsets.UTF_8));
        assertEquals(0, follower.exitValue());
        assertEquals(30, lines.size(), lines.toString());
        assertEquals("0 schema=1 local_ts_ns=1000 payload=tick 0", lines.get(0));
        assertEquals("29 schema=1 local_ts_ns=1029 payload=tick 29", lines.get(29));
    }

    // ---- Released-format fixtures (tests/compat, ADR 0033) ----

    /** Decodes a TTFXPACK v1 file (format in tests/compat/pack.py) into the directory {@code out}. */
    static void unpack(Path pack, Path out) throws IOException {
        ByteBuffer b = ByteBuffer.wrap(Files.readAllBytes(pack)).order(ByteOrder.LITTLE_ENDIAN);
        byte[] magic = new byte[8];
        b.get(magic);
        assertEquals("TTFXPACK", new String(magic, StandardCharsets.US_ASCII));
        assertEquals(1, b.getInt(), "pack version");
        int files = b.getInt();
        Files.createDirectories(out);
        for (int f = 0; f < files; f++) {
            byte[] name = new byte[Short.toUnsignedInt(b.getShort())];
            b.get(name);
            long length = b.getLong();
            byte[] contents = new byte[Math.toIntExact(length)];
            int runs = b.getInt();
            for (int r = 0; r < runs; r++) {
                int offset = Math.toIntExact(b.getLong());
                int len = b.getInt();
                b.get(contents, offset, len);
            }
            Path file = out.resolve(new String(name, StandardCharsets.UTF_8));
            Files.createDirectories(file.getParent());
            Files.write(file, contents);
        }
        assertEquals(0, b.remaining(), "trailing bytes in " + pack);
    }

    private static String digest(List<RecordCopy> records) throws Exception {
        MessageDigest h = MessageDigest.getInstance("SHA-256");
        ByteBuffer meta = ByteBuffer.allocate(36).order(ByteOrder.LITTLE_ENDIAN);
        for (RecordCopy r : records) {
            meta.clear();
            meta.putLong(r.seq()).putInt(r.schemaId()).putLong(r.venueTsNs()).putLong(r.localTsNs())
                    .putLong(r.payload().length);
            h.update(meta.array(), 0, 36);
            h.update(r.payload());
        }
        return HexFormat.of().formatHex(h.digest());
    }

    private static List<RecordCopy> readAll(Path dir, boolean durableOnly, boolean mapped) {
        List<RecordCopy> out = new ArrayList<>();
        ReaderOptions o = ReaderOptions.defaults().durableOnly(durableOnly);
        try (Reader r = mapped ? Reader.openMappedUnsafe(dir, o) : Reader.open(dir, o)) {
            for (RecordView v; (v = r.tryNext()) != null; ) {
                out.add(v.copy());
            }
        }
        return out;
    }

    @Test
    void releasedFormatFixturesReadIdenticallyInJava() throws Exception {
        Path fixtures = TestSupport.REPO.resolve("tests/compat/fixtures");
        Map<String, Object> manifest = Json.obj(Json.parse(Files.readString(fixtures.resolve("manifest.json"))));
        assertEquals(1L, Json.u64(manifest.get("format_version")));
        List<Object> entries = Json.arr(manifest.get("fixtures"));
        assertTrue(entries.size() >= 8, "fixtures: " + entries.size());
        int checked = 0;
        for (Object e : entries) {
            Map<String, Object> f = Json.obj(e);
            String name = (String) f.get("name");
            Path dir = tmp.resolve(name);
            unpack(fixtures.resolve((String) f.get("pack")), dir);
            for (Object fileEntry : Json.arr(f.get("files"))) {
                Map<String, Object> file = Json.obj(fileEntry);
                Path p = dir.resolve((String) file.get("name"));
                assertEquals(Json.u64(file.get("len")), Files.size(p), name + "/" + file.get("name"));
                assertEquals(file.get("sha256"), NativeLibrary.sha256(p), name + "/" + file.get("name"));
            }
            for (boolean mapped : new boolean[] {false, true}) {
                String who = name + (mapped ? " (mapped)" : " (safe)");
                List<RecordCopy> all = readAll(dir, false, mapped);
                assertEquals(Json.u64(f.get("records")), all.size(), who);
                assertEquals(Json.u64(f.get("first_seq")), all.get(0).seq(), who);
                assertEquals(Json.u64(f.get("last_seq")), all.get(all.size() - 1).seq(), who);
                assertEquals(f.get("stream_sha256"), digest(all), who + " stream digest");
                List<RecordCopy> durable = readAll(dir, true, mapped);
                assertEquals(Json.u64(f.get("durable_records")), durable.size(), who);
                assertEquals(f.get("durable_stream_sha256"), digest(durable), who + " durable stream digest");
            }
            Map<String, Object> cp = Json.obj(f.get("checkpoint"));
            Checkpoint c = Journal.readCheckpoint(dir).orElseThrow();
            assertEquals(new Checkpoint(Json.u64(cp.get("generation")), Json.u64(cp.get("next_seq")),
                    Json.u64(cp.get("segment_base_seq")), Json.u64(cp.get("offset"))), c, name);
            VerifyReport report = Journal.verify(dir);
            assertTrue(report.isIntact(), name + ": " + report);
            assertEquals(Json.u64(f.get("records")), report.records(), name);

            // A writer recovers the journal (including a torn tail) and continues it without a gap.
            try (Writer w = Writer.open(dir, TestSupport.small())) {
                long next = Json.u64(f.get("next_seq"));
                assertEquals(next, w.nextSeq(), name);
                assertEquals(next, w.append(7, 1, 2, "continued".getBytes(StandardCharsets.UTF_8)), name);
                w.checkpoint();
            }
            List<RecordCopy> after = readAll(dir, true, false);
            assertEquals(Json.u64(f.get("records")) + 1, after.size(), name);
            assertEquals("continued", new String(after.get(after.size() - 1).payload(), StandardCharsets.UTF_8));
            checked++;
        }
        assertEquals(entries.size(), checked);
    }
}
