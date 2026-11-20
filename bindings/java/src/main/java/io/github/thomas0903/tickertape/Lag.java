package io.github.thomas0903.tickertape;

import java.util.OptionalLong;

/**
 * How far a reader is behind what it could read now (ADR 0032).
 *
 * @param records the records it could return now but has not: committed records, or for a durable-only reader,
 *     records covered by the checkpoint marker
 * @param bytes the segment-file footprint between its cursor and its target (headers, seals and unused tails count),
 *     or empty if unknown (a pending seek, a held record, or a segment of either end not listed)
 */
public record Lag(long records, OptionalLong bytes) {}
