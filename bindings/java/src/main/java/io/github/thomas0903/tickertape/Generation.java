package io.github.thomas0903.tickertape;

/** A counter that a reader or batch advances on every call that invalidates the views it handed out. */
final class Generation {
    long value;
}
