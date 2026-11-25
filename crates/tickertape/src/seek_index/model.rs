//! An exhaustive model of seek index builders and writer opens (ADR 0045 → Why the fence is enough): a deterministic
//! explorer visits every reachable state of every interleaving of their steps, depth first, and checks that no index of
//! a history that recovery has replaced is there once a writer has recovered the segment.
//!
//! Plain state enumeration rather than `loom`: the parties share files, not memory, so there are no weak orderings to
//! explore, and the nondeterminism that matters (what a directory listing shows of a rename that runs meanwhile, when a
//! marker is damaged, when a process dies) is the model's own.
//!
//! **This model is a re-implementation of the protocol, not a proof of the code.** The code itself is driven through every
//! interleaving of its fault points by `tests/seek_index.rs` (`no_index_of_an_old_history_survives_any_interleaving`), with
//! one builder and one writer. Only the model has two builders, damage to the marker at any moment (also between a
//! builder's two reads of it; the code has one case of that, `the_marker_behind_the_fence_decides`), and a listing that runs while a name is renamed; the last is also driven in the code through
//! a stub that leaves names out of one listing.
//!
//! One segment `S` is modelled, sealed at first, with two builders and writers opening, dying and opening again. The
//! variants that leave one part of the protocol out show that the model can see the failures the protocol excludes.

use std::collections::HashSet;

/// The checkpoint marker, as far as `S` is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Marker {
    /// It names a later segment: recovery does not change `S`.
    Past,
    /// It names `S` or an earlier segment: `S` is in the recovery domain.
    Older,
    /// There is none: everything is in the recovery domain.
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Builder {
    /// Before the fence (and, where the variant says so, after the marker was read).
    Start {
        eligible: bool,
    },
    Fenced,
    Eligible,
    /// Holds the file with this identity, has read the part of it that a repair changes once.
    Walking {
        file: u8,
        read: u8,
    },
    /// Has read it to the end.
    Walked {
        file: u8,
        read: u8,
    },
    Renamed {
        file: u8,
        read: u8,
    },
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Writer {
    Closed,
    /// Has the lock.
    Locked,
    /// Has read the marker: is `S` in the recovery domain?
    Marked {
        domain: bool,
        pass: u8,
    },
    /// A pass has opened its stream and these names were there.
    Listing {
        domain: bool,
        pass: u8,
        seen: Names,
        maybe: Names,
    },
    /// A pass has read its stream to the end and is unlinking these names.
    Unlinking {
        domain: bool,
        pass: u8,
        names: Names,
    },
    /// Recovery is done; appends, rolls and checkpoints follow.
    Running,
}

/// Which names exist: the temporary files of the two builders (bit `n` for each builder's `n`th, whose name is never
/// used again), and the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
struct Names {
    tmp: [u8; 2],
    index: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct State {
    marker: Marker,
    sealed: bool,
    /// Counts the changes recovery made to `S`: the history an index is of.
    history: u8,
    /// Identity of the file that is `S`.
    file: u8,
    /// As in [`Names`]; at most one of each builder.
    tmp: [u8; 2],
    /// The history the index that is there was built from, and whose temporary file it was.
    index: Option<(u8, u8)>,
    builders: [Builder; 2],
    restarts: [u8; 2],
    writer: Writer,
    /// Writer opens, marker damages and repairs left to explore.
    opens: u8,
    damages: u8,
    repairs: u8,
}

/// What of the protocol is in place.
#[derive(Debug, Clone, Copy)]
struct Protocol {
    fence_before_marker: bool,
    passes: u8,
    /// Without a marker, the lock is probed.
    probe: bool,
    /// A removed temporary file makes the rename fail.
    tmp_removed: bool,
}

const FULL: Protocol = Protocol {
    fence_before_marker: true,
    passes: 2,
    probe: true,
    tmp_removed: true,
};

impl State {
    fn names(&self) -> Names {
        Names {
            tmp: self.tmp,
            index: self.index.is_some(),
        }
    }

    /// The rule: once a writer has recovered, the index, if there is one, is of the segment's present history.
    fn holds(&self) -> bool {
        self.writer != Writer::Running || self.index.is_none_or(|(h, _)| h == self.history)
    }

    fn successors(&self, p: Protocol, out: &mut Vec<State>) {
        for i in 0..2 {
            self.builder(p, i, out);
        }
        self.writer(p, out);
        // Damage, at any moment: the newest slot lost, or both.
        if self.damages > 0 {
            for marker in [Marker::Older, Marker::None] {
                let moves = match self.marker {
                    Marker::Past => true,
                    Marker::Older => marker == Marker::None,
                    Marker::None => false,
                };
                if moves {
                    out.push(State {
                        marker,
                        damages: self.damages - 1,
                        ..*self
                    });
                }
            }
        }
        // The writer dies at any step.
        if self.writer != Writer::Closed {
            out.push(State {
                writer: Writer::Closed,
                ..*self
            });
        }
    }

    /// Whether a builder reading the marker now goes on.
    fn eligible(&self, p: Protocol) -> bool {
        match self.marker {
            Marker::Past => true,
            Marker::Older => false,
            Marker::None => !p.probe || self.writer == Writer::Closed,
        }
    }

    fn with_builder(&self, i: usize, b: Builder) -> State {
        let mut s = *self;
        s.builders[i] = b;
        s
    }

    fn builder(&self, p: Protocol, i: usize, out: &mut Vec<State>) {
        let done = |mut s: State| {
            s.tmp[i] = 0;
            s.builders[i] = Builder::Done;
            s
        };
        match self.builders[i] {
            Builder::Done => return,
            Builder::Start { eligible } => {
                if p.fence_before_marker || eligible {
                    let mut s = self.with_builder(
                        i,
                        if eligible {
                            Builder::Eligible
                        } else {
                            Builder::Fenced
                        },
                    );
                    s.tmp[i] = 1 << s.restarts[i];
                    out.push(s);
                } else if self.eligible(p) {
                    out.push(self.with_builder(i, Builder::Start { eligible: true }));
                } else {
                    out.push(done(*self));
                }
            }
            Builder::Fenced => out.push(if self.eligible(p) {
                self.with_builder(i, Builder::Eligible)
            } else {
                done(*self)
            }),
            Builder::Eligible => out.push(if self.sealed {
                self.with_builder(
                    i,
                    Builder::Walking {
                        file: self.file,
                        read: self.history,
                    },
                )
            } else {
                done(*self)
            }),
            Builder::Walking { file, read } => {
                // A file that recovery replaced keeps its old bytes; one repaired in place shows the new ones, and a
                // walk that meets both does not reach the seal.
                if file != self.file || read == self.history {
                    out.push(self.with_builder(i, Builder::Walked { file, read }));
                } else {
                    out.push(done(*self));
                }
            }
            Builder::Walked { file, read } => {
                if self.tmp[i] != 0 || !p.tmp_removed {
                    let mut s = self.with_builder(i, Builder::Renamed { file, read });
                    s.tmp[i] = 0;
                    s.index = Some((read, i as u8));
                    out.push(s);
                } else if self.restarts[i] > 0 {
                    let mut s = self.with_builder(i, Builder::Start { eligible: false });
                    s.restarts[i] -= 1;
                    out.push(s);
                } else {
                    out.push(done(*self));
                }
            }
            Builder::Renamed { file, .. } => {
                let mut s = done(*self);
                if file != self.file && matches!(self.index, Some((_, owner)) if owner == i as u8) {
                    s.index = None;
                }
                out.push(s);
            }
        }
        // A builder killed at any step leaves what it wrote.
        out.push(self.with_builder(i, Builder::Done));
    }

    fn writer(&self, p: Protocol, out: &mut Vec<State>) {
        match self.writer {
            Writer::Closed => {
                if self.opens > 0 {
                    out.push(State {
                        writer: Writer::Locked,
                        opens: self.opens - 1,
                        ..*self
                    });
                }
            }
            Writer::Locked => out.push(State {
                writer: Writer::Marked {
                    domain: self.marker != Marker::Past,
                    pass: 0,
                },
                ..*self
            }),
            Writer::Marked { domain, pass } if pass < p.passes => out.push(State {
                writer: Writer::Listing {
                    domain,
                    pass,
                    seen: self.names(),
                    maybe: self.names(),
                },
                ..*self
            }),
            Writer::Marked { domain, .. } => {
                let running = |s: State| State {
                    writer: Writer::Running,
                    ..s
                };
                out.push(running(*self));
                if domain && self.repairs > 0 {
                    // In place, or by replacement; what is left is sealed or not.
                    for (file, sealed) in [(0, false), (0, true), (1, false), (1, true)] {
                        out.push(running(State {
                            history: self.history + 1,
                            file: self.file + file,
                            sealed,
                            repairs: self.repairs - 1,
                            ..*self
                        }));
                    }
                    // Killed in the middle of one in place.
                    out.push(State {
                        history: self.history + 1,
                        repairs: self.repairs - 1,
                        writer: Writer::Closed,
                        ..*self
                    });
                }
            }
            Writer::Listing {
                domain,
                pass,
                seen,
                maybe,
            } => {
                // Names there all the time are returned; any that came or went meanwhile, or not: every choice.
                for a in 0..4 {
                    for b in 0..4 {
                        for index in [false, true] {
                            let names = Names {
                                tmp: [
                                    seen.tmp[0] | (maybe.tmp[0] & a),
                                    seen.tmp[1] | (maybe.tmp[1] & b),
                                ],
                                index: seen.index || (maybe.index && index),
                            };
                            out.push(State {
                                writer: Writer::Unlinking {
                                    domain,
                                    pass,
                                    names,
                                },
                                ..*self
                            });
                        }
                    }
                }
            }
            Writer::Unlinking {
                domain,
                pass,
                names,
            } => {
                let mut next = names;
                let mut s = *self;
                // One unlink per step.
                if next.tmp[0] != 0 {
                    s.tmp[0] &= !next.tmp[0];
                    next.tmp[0] = 0;
                } else if next.tmp[1] != 0 {
                    s.tmp[1] &= !next.tmp[1];
                    next.tmp[1] = 0;
                } else if next.index {
                    next.index = false;
                    if domain {
                        s.index = None;
                    }
                }
                s.writer = if next == Names::default() {
                    Writer::Marked {
                        domain,
                        pass: pass + 1,
                    }
                } else {
                    Writer::Unlinking {
                        domain,
                        pass,
                        names: next,
                    }
                };
                out.push(s);
            }
            Writer::Running => {
                // Rolls past `S` and checkpoints: `S` is sealed and the marker is past it.
                if self.marker != Marker::Past || !self.sealed {
                    out.push(State {
                        marker: Marker::Past,
                        sealed: true,
                        ..*self
                    });
                }
            }
        }
    }

    /// A pass that is listing: what came or went since it began is only maybe in its listing.
    fn settle(mut self) -> State {
        let now = self.names();
        if let Writer::Listing { seen, maybe, .. } = &mut self.writer {
            for i in 0..2 {
                seen.tmp[i] &= now.tmp[i];
                maybe.tmp[i] |= now.tmp[i];
            }
            seen.index &= now.index;
            maybe.index |= now.index;
        }
        self
    }
}

/// Every reachable state, from every start; the first that breaks the rule, if any.
fn explore(p: Protocol) -> (usize, Option<State>) {
    let mut seen = HashSet::new();
    let mut stack = Vec::new();
    for marker in [Marker::Past, Marker::Older, Marker::None] {
        stack.push(State {
            marker,
            sealed: true,
            history: 0,
            file: 0,
            tmp: [0; 2],
            index: None,
            builders: [Builder::Start { eligible: false }; 2],
            restarts: [1, 0],
            writer: Writer::Closed,
            opens: 2,
            damages: 1,
            repairs: 2,
        });
    }
    let mut next = Vec::new();
    while let Some(state) = stack.pop() {
        if !seen.insert(state) {
            continue;
        }
        if !state.holds() {
            return (seen.len(), Some(state));
        }
        next.clear();
        state.successors(p, &mut next);
        stack.extend(next.drain(..).map(State::settle));
    }
    (seen.len(), None)
}

#[test]
#[cfg_attr(miri, ignore = "hundreds of thousands of states")]
fn no_index_of_a_replaced_history_survives_a_recovery() {
    let (states, broken) = explore(FULL);
    assert_eq!(broken, None, "after {states} states");
    // Otherwise the model stopped exploring something.
    assert!(states > 100_000, "only {states} states");
}

/// Each part of the protocol is needed: without it the model finds an index of a replaced history.
#[test]
#[cfg_attr(miri, ignore = "hundreds of thousands of states")]
fn the_model_sees_what_the_protocol_excludes() {
    for p in [
        Protocol {
            fence_before_marker: false,
            ..FULL
        },
        Protocol { passes: 1, ..FULL },
        Protocol { passes: 0, ..FULL },
        Protocol {
            probe: false,
            ..FULL
        },
        Protocol {
            tmp_removed: false,
            ..FULL
        },
    ] {
        assert!(explore(p).1.is_some(), "{p:?}");
    }
}
