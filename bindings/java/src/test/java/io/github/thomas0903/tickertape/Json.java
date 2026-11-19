package io.github.thomas0903.tickertape;

import java.math.BigInteger;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/** A minimal JSON reader for the fixture manifest (objects, arrays, strings, integers, booleans, null). */
final class Json {
    private final String s;
    private int i;

    private Json(String s) {
        this.s = s;
    }

    static Object parse(String text) {
        Json j = new Json(text);
        Object v = j.value();
        j.ws();
        if (j.i != text.length()) {
            throw new IllegalArgumentException("trailing data at " + j.i);
        }
        return v;
    }

    private void ws() {
        while (i < s.length() && Character.isWhitespace(s.charAt(i))) {
            i++;
        }
    }

    private Object value() {
        ws();
        char c = s.charAt(i);
        switch (c) {
            case '{': {
                i++;
                Map<String, Object> m = new LinkedHashMap<>();
                ws();
                if (s.charAt(i) == '}') {
                    i++;
                    return m;
                }
                for (;;) {
                    ws();
                    String k = string();
                    ws();
                    expect(':');
                    m.put(k, value());
                    ws();
                    if (s.charAt(i) == ',') {
                        i++;
                        continue;
                    }
                    expect('}');
                    return m;
                }
            }
            case '[': {
                i++;
                List<Object> l = new ArrayList<>();
                ws();
                if (s.charAt(i) == ']') {
                    i++;
                    return l;
                }
                for (;;) {
                    l.add(value());
                    ws();
                    if (s.charAt(i) == ',') {
                        i++;
                        continue;
                    }
                    expect(']');
                    return l;
                }
            }
            case '"':
                return string();
            default:
                if (s.startsWith("true", i)) {
                    i += 4;
                    return Boolean.TRUE;
                }
                if (s.startsWith("false", i)) {
                    i += 5;
                    return Boolean.FALSE;
                }
                if (s.startsWith("null", i)) {
                    i += 4;
                    return null;
                }
                int start = i;
                while (i < s.length() && "-+0123456789.eE".indexOf(s.charAt(i)) >= 0) {
                    i++;
                }
                return new BigInteger(s.substring(start, i));
        }
    }

    private void expect(char c) {
        if (s.charAt(i) != c) {
            throw new IllegalArgumentException("expected " + c + " at " + i);
        }
        i++;
    }

    private String string() {
        expect('"');
        StringBuilder b = new StringBuilder();
        for (;;) {
            char c = s.charAt(i++);
            if (c == '"') {
                return b.toString();
            }
            if (c == '\\') {
                char e = s.charAt(i++);
                switch (e) {
                    case 'n' -> b.append('\n');
                    case 't' -> b.append('\t');
                    case 'u' -> {
                        b.append((char) Integer.parseInt(s.substring(i, i + 4), 16));
                        i += 4;
                    }
                    default -> b.append(e);
                }
            } else {
                b.append(c);
            }
        }
    }

    @SuppressWarnings("unchecked")
    static Map<String, Object> obj(Object o) {
        return (Map<String, Object>) o;
    }

    @SuppressWarnings("unchecked")
    static List<Object> arr(Object o) {
        return (List<Object>) o;
    }

    /** An unsigned 64-bit JSON integer as a Java long with the same bits. */
    static long u64(Object o) {
        return ((BigInteger) o).longValue();
    }
}
