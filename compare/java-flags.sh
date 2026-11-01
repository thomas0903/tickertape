#!/usr/bin/env bash
# OpenHFT/OpenHFT docs/Java-Version-Support.adoc, classpath flags for Java 17+.
# Also exported/used for Aeron's direct buffers. This file is sourced by run.sh.
if [[ -z "${JAVA_HOME:-}" && -d /opt/homebrew/opt/openjdk@21 ]]; then
  export JAVA_HOME=/opt/homebrew/opt/openjdk@21
fi
: "${JAVA_HOME:?Set JAVA_HOME to a Java 21 JDK}"
export PATH="$JAVA_HOME/bin:$PATH"
JAVA_FLAGS=(
  -Xms256m -Xmx256m -XX:+UseParallelGC
  --add-exports=java.base/jdk.internal.misc=ALL-UNNAMED
  --add-exports=java.base/jdk.internal.util=ALL-UNNAMED
  --add-exports=java.base/jdk.internal.ref=ALL-UNNAMED
  --add-exports=java.base/sun.nio.ch=ALL-UNNAMED
  --add-exports=jdk.unsupported/sun.misc=ALL-UNNAMED
  --add-exports=jdk.compiler/com.sun.tools.javac.file=ALL-UNNAMED
  --add-opens=jdk.compiler/com.sun.tools.javac=ALL-UNNAMED
  --add-opens=java.base/java.lang=ALL-UNNAMED
  --add-opens=java.base/java.lang.reflect=ALL-UNNAMED
  --add-opens=java.base/java.io=ALL-UNNAMED
  --add-opens=java.base/java.util=ALL-UNNAMED
  -Dchronicle.analytics.disable=true
  -Dorg.slf4j.simpleLogger.defaultLogLevel=warn
)
