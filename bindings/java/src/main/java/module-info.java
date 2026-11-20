/**
 * The tickertape Java binding: write, follow and replay tickertape journals from Java through the C ABI, with the
 * Foreign Function and Memory API. Start with {@link io.github.thomas0903.tickertape.Writer} and
 * {@link io.github.thomas0903.tickertape.Reader}; {@code docs/java-api.md} explains the rules they share.
 *
 * <p>The module calls restricted methods of {@code java.lang.foreign}. Run with
 * {@code --enable-native-access=io.github.thomas0903.tickertape} (module path) or
 * {@code --enable-native-access=ALL-UNNAMED} (class path).
 */
@SuppressWarnings("module") // "thomas0903" ends in digits: it is the GitHub user name in the group id
module io.github.thomas0903.tickertape {
    exports io.github.thomas0903.tickertape;
}
