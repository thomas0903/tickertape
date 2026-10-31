# 0011: Optional Binance trade recorder

- **Status:** accepted
- **Date:** 2026-10-31

## Context

The examples milestone permits an optional live feed while keeping the synthetic demo and ordinary builds
independent of network clients. Live trades must use the existing [market demo schema](../../examples/README.md).

## Options considered

1. An asynchronous client: adds an executor for a single stream.
2. A synchronous `tungstenite` client with native TLS: uses platform certificate validation and needs no new
   licence exceptions. Linux builds need OpenSSL development packages and `pkg-config`.

## Decision

Gate the binary, `tungstenite`, `serde_json` and `ctrlc` behind an off-by-default `live` feature. Also use
`serde_json` as a test-only dependency so hand-written trade fixtures run in ordinary offline unit tests.
Use Binance's public combined trade stream, with an optional URL override and no credentials.

Map symbols to their zero-based position in `--symbols`; users must preserve that order when appending to an
existing journal. Record the taker's side (`buy = !buyer_is_maker`), trade id, exchange milliseconds converted
with checked multiplication to nanoseconds, and the local receipt time. Parse signed decimal strings directly
to integers scaled by 100,000,000: reject overflow and nonzero digits beyond eight fractional places; accept
extra trailing zeros because they preserve the exact value. Reject nonpositive live prices and quantities.

Bound frame/message sizes and socket I/O waits. Count and skip invalid or unselected trade messages. Flush
automatic pong replies. Reconnect with exponential backoff capped at 30 seconds; keep sequence numbers advancing
but make no claim of gap-free capture, replay, or deduplication across reconnects. Checkpoint every configured
batch and on every normal, signal, limit or recoverable-error exit. Journal write/checkpoint errors remain fatal.
Check stop flags during reads and backoff; DNS resolution still uses the system's blocking resolver.

## Consequences

No format or library API changes. `examples/README.md` documents the mapping, precision policy, capture gaps,
terms and optional dependencies. CI only builds the optional binary; parsing and loopback tests need no external
service. Default workspace builds do not compile the live network dependencies.
