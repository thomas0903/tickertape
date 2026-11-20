package io.github.thomas0903.tickertape;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotEquals;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assertions.fail;

import java.io.IOException;
import java.lang.foreign.AddressLayout;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.StructLayout;
import java.lang.foreign.ValueLayout;
import java.nio.file.Files;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.TreeSet;
import java.util.regex.Matcher;
import java.util.regex.Pattern;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.Test;

/**
 * The C header is the source of truth: these tests fail when it gains a function, status code, corruption reason or
 * struct field that the binding has not decided about, or when a descriptor, layout or version differs from it.
 */
class HeaderTest {
    private static String header;
    /** The header without comments and preprocessor lines. */
    private static String code;

    @BeforeAll
    static void readHeader() throws IOException {
        header = Files.readString(TestSupport.HEADER);
        String noComments = header.replaceAll("(?s)/\\*.*?\\*/", " ").replaceAll("//[^\n]*", " ");
        StringBuilder b = new StringBuilder();
        for (String line : noComments.split("\n")) {
            String t = line.strip();
            if (t.startsWith("#") || t.equals("extern \"C\" {") || t.equals("}")) {
                continue;
            }
            b.append(line).append('\n');
        }
        code = b.toString();
    }

    /** Every function declared in the header, with its C return type and parameter types. */
    private static Map<String, List<String>> headerFunctions() {
        Map<String, List<String>> out = new LinkedHashMap<>();
        Pattern decl = Pattern.compile("^(.*?)\\b(tickertape_\\w+)\\s*\\((.*)\\)$", Pattern.DOTALL);
        for (String statement : code.split(";")) {
            String s = statement.replaceAll("\\s+", " ").strip();
            if (s.startsWith("typedef") || s.startsWith("enum") || s.contains("{")) {
                // Typedefs (including the fill function pointer), enums and struct bodies.
                int brace = s.lastIndexOf('}');
                if (brace < 0) {
                    continue;
                }
                s = s.substring(brace + 1).strip();
            }
            Matcher m = decl.matcher(s);
            if (!m.matches()) {
                continue;
            }
            List<String> types = new ArrayList<>();
            types.add(m.group(1).strip());
            String params = m.group(3).strip();
            if (!params.equals("void")) {
                for (String p : params.split(",")) {
                    String t = p.strip();
                    // Drop the parameter name: the last identifier, unless the type ends in '*'.
                    t = t.replaceAll("\\s*\\b\\w+$", "").strip();
                    types.add(t);
                }
            }
            out.put(m.group(2), types);
        }
        return out;
    }

    private static String layoutTag(MemoryLayout l) {
        if (l instanceof AddressLayout || l.equals(Signatures.POINTER_AS_LONG)) {
            return "ptr";
        }
        if (l instanceof ValueLayout.OfInt) {
            return "i32";
        }
        if (l instanceof ValueLayout.OfLong) {
            return "i64";
        }
        return l.toString();
    }

    private static String cTag(String type) {
        String t = type.replace("const ", "").replace("struct ", "").strip();
        if (t.endsWith("*") || t.equals("tickertape_fill_fn")) {
            return "ptr";
        }
        return switch (t) {
            case "uint32_t", "int32_t", "tickertape_status" -> "i32";
            case "uint64_t", "int64_t", "size_t" -> "i64";
            case "void" -> "void";
            default -> throw new AssertionError("unmapped C type in the header: '" + type + "'");
        };
    }

    @Test
    void everyHeaderFunctionIsWrappedOrDeliberatelyNot() throws IOException {
        Set<String> declared = headerFunctions().keySet();
        assertTrue(declared.size() > 40, "parsed only " + declared);
        Set<String> decided = new HashSet<>(Signatures.FUNCTIONS.keySet());
        for (String n : Signatures.NOT_WRAPPED) {
            assertTrue(decided.add(n), n + " is both wrapped and listed as not wrapped");
        }
        Set<String> undecided = new TreeSet<>(declared);
        undecided.removeAll(decided);
        assertTrue(undecided.isEmpty(), "header functions with no decision in the Java binding (wrap them in"
                + " Signatures.FUNCTIONS, or add them to Signatures.NOT_WRAPPED and ADR 0037 with the reason): " + undecided);
        Set<String> stale = new TreeSet<>(decided);
        stale.removeAll(declared);
        assertTrue(stale.isEmpty(), "functions the binding knows but the header does not declare: " + stale);

        String adr = Files.readString(TestSupport.REPO.resolve("docs/decisions/0037-java-binding.md"));
        for (String n : Signatures.NOT_WRAPPED) {
            assertTrue(adr.contains("`" + n + "`"), "ADR 0037 must say why " + n + " is not wrapped");
        }
    }

    @Test
    void descriptorsMatchTheHeader() {
        Map<String, List<String>> declared = headerFunctions();
        for (Map.Entry<String, FunctionDescriptor> e : Signatures.FUNCTIONS.entrySet()) {
            List<String> c = declared.get(e.getKey());
            assertNotNull(c, e.getKey());
            FunctionDescriptor d = e.getValue();
            List<String> expected = new ArrayList<>();
            for (String t : c) {
                expected.add(cTag(t));
            }
            List<String> actual = new ArrayList<>();
            actual.add(d.returnLayout().map(HeaderTest::layoutTag).orElse("void"));
            for (MemoryLayout p : d.argumentLayouts()) {
                actual.add(layoutTag(p));
            }
            assertEquals(expected, actual, "descriptor of " + e.getKey() + " (return type first) against " + c);
        }
        // The fill callback type.
        Matcher m = Pattern.compile("typedef (\\w+) \\(\\*tickertape_fill_fn\\)\\(([^)]*)\\);").matcher(header);
        assertTrue(m.find(), "tickertape_fill_fn typedef");
        List<String> expected = new ArrayList<>(List.of(cTag(m.group(1))));
        for (String p : m.group(2).split(",")) {
            expected.add(cTag(p.strip().replaceAll("\\s*\\b\\w+$", "")));
        }
        List<String> actual = new ArrayList<>(List.of(layoutTag(Signatures.FILL_FN.returnLayout().orElseThrow())));
        Signatures.FILL_FN.argumentLayouts().forEach(l -> actual.add(layoutTag(l)));
        assertEquals(expected, actual, "tickertape_fill_fn");
    }

    @Test
    void structLayoutsMatchTheHeader() {
        Pattern struct = Pattern.compile("typedef struct (\\w+) \\{([^}]*)\\} \\1;");
        Matcher m = struct.matcher(code.replaceAll("\\s+", " "));
        Set<String> seen = new HashSet<>();
        while (m.find()) {
            String name = m.group(1);
            StructLayout java = Signatures.STRUCTS.get(name);
            assertNotNull(java, "the binding has no layout for " + name);
            seen.add(name);
            long offset = 0;
            List<MemoryLayout> members = java.memberLayouts();
            String[] fields = m.group(2).strip().split(";");
            int i = 0;
            for (String f : fields) {
                String field = f.strip();
                if (field.isEmpty()) {
                    continue;
                }
                String fieldName = field.replaceAll(".*?\\b(\\w+)$", "$1");
                String tag = cTag(field.replaceAll("\\s*\\b\\w+$", ""));
                long size = tag.equals("i32") ? 4 : 8;
                offset = (offset + size - 1) / size * size; // natural alignment
                assertTrue(i < members.size(), name + " has more fields in the header than in Java: " + fieldName);
                MemoryLayout member = members.get(i++);
                assertEquals(fieldName, member.name().orElseThrow(), name + " field " + i);
                assertEquals(tag, layoutTag(member), name + "." + fieldName);
                assertEquals(offset, java.byteOffset(MemoryLayout.PathElement.groupElement(fieldName)), name + "." + fieldName);
                offset += size;
            }
            assertEquals(members.size(), i, name + " has fewer fields in the header than in Java");
            assertEquals((offset + 7) / 8 * 8, java.byteSize(), "size of " + name);
        }
        assertEquals(Signatures.STRUCTS.keySet(), seen, "structs in the header");
        // The frozen sizes from ADR 0028 (asserted in Rust and C too).
        assertEquals(48, Signatures.RECORD.byteSize());
        assertEquals(40, Signatures.NEW_RECORD.byteSize());
        assertEquals(32, Signatures.SEGMENT_INFO.byteSize());
        assertEquals(32, Signatures.CHECKPOINT.byteSize());
        assertEquals(72, Signatures.VERIFY_REPORT.byteSize());
        assertEquals(32, Signatures.RETENTION_REPORT.byteSize());
        assertEquals(272, Signatures.JOURNAL_STATUS_REPORT.byteSize());
        assertEquals(120, Signatures.BEACON_STATUS.byteSize());
        assertEquals(112, Signatures.WRITER_METRICS.byteSize());
        assertEquals(72, Signatures.READER_METRICS.byteSize());
        assertEquals(56, Signatures.CHECKPOINT_PROGRESS.byteSize());
    }

    /** The enumerators of a C enum, by name. */
    private static Map<String, Integer> enumerators(String enumName) {
        Matcher m = Pattern.compile("enum " + enumName + "\\s*(?::\\s*\\w+\\s*)?\\{([^}]*)\\}").matcher(code);
        assertTrue(m.find(), "enum " + enumName);
        Map<String, Integer> out = new LinkedHashMap<>();
        for (String e : m.group(1).split(",")) {
            String t = e.strip();
            if (t.isEmpty()) {
                continue;
            }
            String[] kv = t.split("=");
            out.put(kv[0].strip(), Integer.parseInt(kv[1].strip()));
        }
        return out;
    }

    @Test
    void everyStatusCodeMapsToItsOwnException() {
        Map<String, Integer> codes = enumerators("tickertape_status_code");
        assertTrue(codes.size() >= 20, codes.toString());
        Map<Class<?>, String> classes = new HashMap<>();
        for (Map.Entry<String, Integer> e : codes.entrySet()) {
            Status s = Status.of(e.getValue());
            assertEquals(e.getKey(), s.cName(), "Status for " + e);
            assertEquals((int) e.getValue(), s.code());
            if (e.getValue() >= 0) {
                continue;
            }
            TickertapeException x = Errors.create(e.getValue(), "message", 5, 8, 42, null);
            assertEquals(s, x.status(), e.getKey());
            assertEquals((int) e.getValue(), x.code());
            assertEquals("message", x.getMessage());
            if (s == Status.UNKNOWN) {
                assertEquals(TickertapeException.class, x.getClass());
            } else {
                assertNotEquals(TickertapeException.class, x.getClass(), e.getKey() + " has no exception class of its own");
                String previous = classes.put(x.getClass(), e.getKey());
                assertEquals(null, previous, x.getClass() + " maps both " + previous + " and " + e.getKey());
            }
        }
        for (Status s : Status.values()) {
            assertTrue(codes.containsKey(s.cName()), s + " is not in the header");
        }
        // Details carried by the exceptions.
        assertEquals(5, ((JournalIOException) Errors.create(-2, "", 5, 0, -1, null)).osErrorCode());
        assertEquals(28, ((StorageFullException) Errors.create(-3, "", 28, 0, -1, null)).osErrorCode());
        CorruptJournalException c = (CorruptJournalException) Errors.create(-5, "", 0, 8, -1, null);
        assertEquals(CorruptReason.BAD_CHECKSUM, c.reason());
        assertEquals(8, c.reasonCode());
        assertEquals(42, ((BufferTooSmallException) Errors.create(-14, "", 0, 0, 42, null)).requiredBytes());
        // A code this binding does not know (a newer library) is still an error, with its raw code.
        TickertapeException future = Errors.create(-19, "new", 0, 0, -1, null);
        assertEquals(Status.UNKNOWN, future.status());
        assertEquals(-19, future.code());
    }

    @Test
    void everyCorruptReasonIsMapped() {
        Map<String, Integer> reasons = enumerators("tickertape_corrupt_reason");
        assertTrue(reasons.size() >= 17, reasons.toString());
        for (Map.Entry<String, Integer> e : reasons.entrySet()) {
            CorruptReason r = CorruptReason.of(e.getValue());
            assertEquals(e.getKey(), r.cName(), "no CorruptReason for " + e);
            assertEquals((int) e.getValue(), r.code());
        }
        for (CorruptReason r : CorruptReason.values()) {
            assertTrue(reasons.containsKey(r.cName()), r + " is not in the header");
        }
        // A reason this binding does not know (a newer library).
        assertEquals(CorruptReason.UNKNOWN, CorruptReason.of(16));
    }

    @Test
    void otherEnumsMatch() {
        Map<String, Integer> policies = enumerators("tickertape_recovery_policy");
        assertEquals(RecoveryPolicy.values().length, policies.size());
        for (RecoveryPolicy p : RecoveryPolicy.values()) {
            assertEquals(p.code, policies.get("TICKERTAPE_RECOVERY_" + p.name()), p.toString());
        }
        assertEquals(Map.of("TICKERTAPE_HELPER_CPUS_PROCESS", HelperCpus.PROCESS, "TICKERTAPE_HELPER_CPUS_INHERITED",
                HelperCpus.INHERITED, "TICKERTAPE_HELPER_CPUS_LIST", HelperCpus.LIST),
                enumerators("tickertape_helper_cpus"));
        Map<String, Integer> timeouts = enumerators("tickertape_timeout");
        assertEquals(Map.of("TICKERTAPE_NO_WAIT", 0, "TICKERTAPE_WAIT_FOREVER", -1), timeouts,
                "the binding passes 0 for no wait and only finite slices otherwise");
    }

    /** The metrics enumerations (ADR 0032): every C value has a Java value of the same name and code, and back. */
    @Test
    void everyMetricsEnumeratorIsMapped() {
        record E(String cEnum, Class<? extends Enum<?>> java, java.util.function.IntFunction<Enum<?>> of,
                java.util.function.Function<Enum<?>, String> cName, java.util.function.ToIntFunction<Enum<?>> code) {}
        List<E> enums = List.of(
                new E("tickertape_head_end", HeadEnd.class, HeadEnd::of, e -> ((HeadEnd) e).cName(), e -> ((HeadEnd) e).code()),
                new E("tickertape_checkpoint_problem", CheckpointProblem.class, CheckpointProblem::of,
                        e -> ((CheckpointProblem) e).cName(), e -> ((CheckpointProblem) e).code()),
                new E("tickertape_age_unknown", AgeUnknown.class, AgeUnknown::of, e -> ((AgeUnknown) e).cName(),
                        e -> ((AgeUnknown) e).code()),
                new E("tickertape_lag_unknown", LagUnknown.class, LagUnknown::of, e -> ((LagUnknown) e).cName(),
                        e -> ((LagUnknown) e).code()),
                new E("tickertape_beacon_state", BeaconState.class, BeaconState::of, e -> ((BeaconState) e).cName(),
                        e -> ((BeaconState) e).code()),
                new E("tickertape_readers_dir_state", ReadersDirState.class, ReadersDirState::of,
                        e -> ((ReadersDirState) e).cName(), e -> ((ReadersDirState) e).code()));
        for (E e : enums) {
            Map<String, Integer> c = enumerators(e.cEnum());
            assertTrue(c.size() >= 3, e.cEnum() + ": " + c);
            for (Map.Entry<String, Integer> v : c.entrySet()) {
                Enum<?> j = e.of().apply(v.getValue());
                assertEquals(v.getKey(), e.cName().apply(j), e.cEnum() + " " + v);
                assertEquals((int) v.getValue(), e.code().applyAsInt(j));
            }
            for (Enum<?> j : e.java().getEnumConstants()) {
                assertTrue(c.containsKey(e.cName().apply(j)), j + " is not in the header");
            }
        }
        assertEquals(HeadEnd.OTHER, HeadEnd.of(7));
        assertEquals(BeaconState.OTHER, BeaconState.of(7));
        assertEquals(ReadersDirState.OTHER, ReadersDirState.of(7));
        assertEquals(LagUnknown.OTHER_REASON, LagUnknown.of(7));
    }

    private static int define(String name) {
        Matcher m = Pattern.compile("#define " + name + " (\\d+)\\n").matcher(header);
        if (!m.find()) {
            fail("#define " + name);
        }
        return Integer.parseInt(m.group(1));
    }

    @Test
    void versionsMatchTheHeaderAndThePom() throws IOException {
        int major = define("TICKERTAPE_VERSION_MAJOR");
        int minor = define("TICKERTAPE_VERSION_MINOR");
        int patch = define("TICKERTAPE_VERSION_PATCH");
        assertEquals(define("TICKERTAPE_ABI_VERSION"), NativeLibrary.ABI_VERSION, "ABI version");
        assertEquals(major * 1_000_000 + minor * 1000 + patch, NativeLibrary.VERSION_NUMBER, "version number");
        assertEquals(major + "." + minor + "." + patch, NativeLibrary.VERSION, "binding version");
        String pom = Files.readString(TestSupport.REPO.resolve("bindings/java/pom.xml"));
        Matcher m = Pattern.compile("<artifactId>tickertape</artifactId>\\s*<version>([^<]+)</version>").matcher(pom);
        assertTrue(m.find(), "project version in pom.xml");
        assertEquals(NativeLibrary.VERSION, m.group(1), "pom.xml version must be the C library's version");
        assertEquals(NativeLibrary.VERSION, NativeLibrary.versionString(NativeLibrary.VERSION_NUMBER));
    }

    @Test
    void compatibilityRule() {
        int v = NativeLibrary.VERSION_NUMBER;
        assertTrue(NativeLibrary.compatible(NativeLibrary.ABI_VERSION, v));
        assertFalse(NativeLibrary.compatible(NativeLibrary.ABI_VERSION + 1, v), "another ABI version");
        if (NativeLibrary.ABI_VERSION == 0) {
            assertFalse(NativeLibrary.compatible(0, v + 1), "before 1.0 a newer library is refused");
            assertFalse(NativeLibrary.compatible(0, v - 1), "before 1.0 an older library is refused");
        } else {
            assertTrue(NativeLibrary.compatible(NativeLibrary.ABI_VERSION, v + 1), "from 1.0 a newer library is fine");
            assertFalse(NativeLibrary.compatible(NativeLibrary.ABI_VERSION, v - 1), "an older library is refused");
        }
    }
}
