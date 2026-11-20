# tickertape for Java

The Java binding for tickertape: write, follow live and replay journals from Java (JDK 25 or newer), through the C
ABI and the Foreign Function and Memory API, with no JNI and no runtime dependencies. The API guide is
[`docs/java-api.md`](../../docs/java-api.md); the design is [ADR 0037](../../docs/decisions/0037-java-binding.md).

Not published to Maven Central yet, and not stable before tickertape 1.0. The coordinates are
`io.github.thomas0903:tickertape` (module `io.github.thomas0903.tickertape`); the release workflow publishes them once
the owner enables Maven Central ([`docs/install.md`](../../docs/install.md#java-maven-central)).

## Build

```bash
cargo build --release -p tickertape-ffi                  # from the repository root: target/release/libtickertape.*
mvn -f bindings/java/pom.xml package -DskipTests         # bindings/java/target/tickertape-0.4.0.jar
```

To run the tests, build the CLI and the C test programs too, then `mvn verify` (with `JAVA_HOME` pointing at a JDK 25):

```bash
bindings/java/scripts/build-native.sh
mvn -f bindings/java/pom.xml verify
```

## Example

[`examples/Example.java`](examples/Example.java) records, follows live from another process, and replays. It runs from
source with the `java` launcher (on Linux the library is `libtickertape.so`):

```bash
cd bindings/java
example() {
    java --enable-native-access=ALL-UNNAMED -cp target/tickertape-0.4.0.jar \
        -Dtickertape.library.path=../../target/release/libtickertape.dylib examples/Example.java "$@"
}
example follow /tmp/tt-java 20 &        # waits for the writer to create the journal
example record /tmp/tt-java 20 50       # 20 records, one every 50 ms
wait
example replay /tmp/tt-java 10          # records 10..19
TICKERTAPE_DURABLE_ONLY=1 example replay /tmp/tt-java
```

`ExampleTest` runs these steps in the test suite, and CI runs them again from the packaged JARs only.

## Layout

| Path | What |
|---|---|
| `src/main/java` | The binding: one exported package, `io.github.thomas0903.tickertape` |
| `src/test/java` | JUnit tests, including the header checks and the cross-language fixtures |
| `examples/Example.java` | The runnable example |
| `scripts/build-native.sh` | Builds the library, the CLI and the C test programs the tests use |
| `scripts/stage-natives.sh` | Stages one platform's library for its natives JAR (`mvn -Pnatives package`; add `-Prelease` for the sources and Javadoc JARs) |
