package io.github.thomas0903.tickertape;

/**
 * What {@link Journal#applyRetention} removed (or, with {@code dryRun}, would remove).
 *
 * @param removedSegments segment files removed (planned for removal, with {@code dryRun})
 * @param removedBytes the sum of their full sizes
 * @param alreadyRemovedSegments planned files that were already gone (removed concurrently); 0 with {@code dryRun}
 */
public record RetentionReport(long removedSegments, long removedBytes, long alreadyRemovedSegments) {}
