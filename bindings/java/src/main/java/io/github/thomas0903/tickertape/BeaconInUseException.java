package io.github.thomas0903.tickertape;

/**
 * A reader beacon with this name is held by a running reader (ADR 0032). No function of the C ABI creates beacons
 * yet, so the binding does not throw this today; the class exists so that every C status has its own exception.
 * Status {@link Status#BEACON_IN_USE}.
 */
public final class BeaconInUseException extends TickertapeException {
    private static final long serialVersionUID = 1L;

    BeaconInUseException(String message, Throwable cause) {
        super(Status.BEACON_IN_USE, Status.BEACON_IN_USE.code(), message, cause);
    }
}
