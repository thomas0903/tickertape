//! Publish recorded messages back onto iceoryx2 services.
//!
//! [`Replayer`] reads a recording from any sequence number and sends each message with the user header and
//! payload bytes it was recorded with, either as fast as possible or at the recorded pace scaled by a speed
//! factor. The pace comes from the differences between records' `local_ts_ns` (the receive times): each send is
//! scheduled relative to the first replayed record, the replayer sleeps on a monotonic clock and spins for the
//! last millisecond ([`ReplayerBuilder::spin`]), and the [`PacingReport`] says how late sends were, scheduled versus actual.
//!
//! # Service names
//!
//! By default every recorded service is replayed onto `replay/<name>`, so a replay can't feed a recorder of the
//! original service (which would record its own replay). Replaying onto the original names needs
//! [`Remap::original_names`]; individual names can be mapped with [`Remap::rule`].
//!
//! A replay can still feed a recorder that is recording the target name right now (for example `--original-names`
//! while the recorder that wrote the journal still runs, or a journal that recorded a `replay/...` service). So, by
//! default, the replay ends at the journal's committed end as it was when the replay started, and while a writer
//! holds the journal the replayer refuses a target name that any descriptor in the journal names, unless
//! [`ReplayerBuilder::allow_live_targets`] is given. It learns those names by reading the whole journal once before
//! the replay, so where the descriptors lie doesn't matter; a service the recorder hasn't opened yet (still waiting
//! for it to appear) has no descriptor and isn't known, and a replay that creates it is recorded, once.
//!
//! # Types
//!
//! Each target service is opened, or created with the recorded settings, using the recorded type details, which
//! were validated when their descriptor was decoded. If it already exists with different types, the replayer
//! refuses with [`Error::Incompatible`] and says what differs. Every later descriptor that resolves to the same
//! target (a later recording run of the service, or another service mapped onto it) must have exactly the same
//! payload and user header types (variant, name, size and alignment), or the replay stops with
//! [`Error::Incompatible`] naming both.
//!
//! # Trust
//!
//! A subscriber of a target service reads the replayed bytes as values of the advertised types. Services registered
//! with [`ReplayerBuilder::typed`] use only iceoryx2's public API, check the Rust types against the recording, and
//! rebuild every value with `zerocopy::FromBytes`, so [`Replayer::run`] is safe. Replaying any other service sends
//! the recorded bytes under the recorded type name without knowing the type, so a typed subscriber of a type with
//! invalid bit patterns (`bool`, an enum) would have undefined behaviour if the bytes are not valid values. The
//! journal's checksums protect against accidental damage, not against a journal written with wrong contents. That
//! path is `Replayer::run_dynamic`, an `unsafe fn` (feature `dynamic`): replay only recordings you trust, as with
//! upstream's own replay tool.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use iceoryx2::port::publisher::Publisher;
use iceoryx2::prelude::*;
use serde::Serialize;
use tickertape::{Reader, ReaderConfig};

use crate::layout::{self, Decoded, Descriptor, TypeInfo};
use crate::pacing::Pacer;
pub use crate::pacing::{MAX_SPEED, MIN_SPEED, PacingReport};
use crate::recorder::{Signals, describe, service_name, service_version};
use crate::typed::{Create, PortBudget, ReplayableHeader, header_type};
use crate::{Catalog, Error, Replayable};

/// The default prefix for replayed service names.
pub const DEFAULT_PREFIX: &str = "replay/";

/// How recorded service names map to the names replayed onto.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remap {
    prefix: String,
    rules: BTreeMap<String, String>,
    allow_original: bool,
}

impl Default for Remap {
    /// Prefix every name with [`DEFAULT_PREFIX`].
    fn default() -> Self {
        Remap {
            prefix: DEFAULT_PREFIX.into(),
            rules: BTreeMap::new(),
            allow_original: false,
        }
    }
}

impl Remap {
    /// Prefix names without a rule with `prefix` instead of [`DEFAULT_PREFIX`].
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// Replay names without a rule onto the original service names, and allow rules that map a name to itself.
    /// A recorder of those services would record the replay too.
    pub fn original_names(mut self) -> Self {
        self.prefix.clear();
        self.allow_original = true;
        self
    }

    /// Replay the recorded service `from` onto `to`.
    pub fn rule(mut self, from: impl Into<String>, to: impl Into<String>) -> Self {
        self.rules.insert(from.into(), to.into());
        self
    }

    /// The name `recorded` is replayed onto.
    ///
    /// # Errors
    ///
    /// [`Error::Usage`] if that is the recorded name itself and [`Remap::original_names`] wasn't given.
    pub fn target(&self, recorded: &str) -> Result<String, Error> {
        let target = match self.rules.get(recorded) {
            Some(t) => t.clone(),
            None => format!("{}{recorded}", self.prefix),
        };
        if target == recorded && !self.allow_original {
            return Err(Error::Usage(format!(
                "refusing to replay onto the recorded service name \"{recorded}\" without an explicit request \
                 (original names); a recorder of that service would record the replay"
            )));
        }
        Ok(target)
    }
}

/// An open publisher for one target service.
pub(crate) trait Sink {
    fn send(&mut self, user_header: &[u8], payload: &[u8]) -> Result<(), Error>;
    fn subscribers(&self) -> usize;
}

type SinkFactory =
    Box<dyn FnMut(&Node<ipc::Service>, &str, &Descriptor) -> Result<Box<dyn Sink>, Error> + Send>;

/// Opens a sink for a service that isn't registered as typed. Only `Replayer::run_dynamic` passes one.
pub(crate) type UntypedSink =
    fn(&Node<ipc::Service>, &str, &Descriptor) -> Result<Box<dyn Sink>, Error>;

/// Why [`Replayer::run`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ReplayEnd {
    /// It reached the end of the journal: the committed end when the replay started (the default), or the end when
    /// the replayer got there ([`ReplayerBuilder::end_at_start`] set to false).
    EndOfJournal,
    /// It passed [`ReplayerBuilder::to`].
    ToReached,
    /// `SIGINT` or `SIGTERM`.
    Signal,
    /// The stop flag was set.
    StopFlag,
}

/// Per-service results of a replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReplayServiceReport {
    /// The recorded service name.
    pub recorded: String,
    /// The name replayed onto.
    pub target: String,
    /// Messages sent.
    pub messages: u64,
}

/// The results of [`Replayer::run`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReplayReport {
    /// First sequence number replayed, if any.
    pub first_seq: Option<u64>,
    /// Last sequence number read.
    pub last_seq: Option<u64>,
    /// Messages sent.
    pub messages: u64,
    /// Data records skipped by the service filter.
    pub filtered: u64,
    /// Data and loss records skipped because the descriptor they name is no longer in the journal (retention or
    /// archiving removed its segment).
    pub undescribed: u64,
    /// Other records passed over (descriptors, loss records with their descriptor, fillers, other schemas).
    pub other_records: u64,
    /// Per target service.
    pub services: Vec<ReplayServiceReport>,
    /// Pacing, if paced.
    pub pacing: Option<PacingReport>,
    /// Why it stopped.
    pub end: ReplayEnd,
}

/// Configures a [`Replayer`]. Start with [`Replayer::builder`].
pub struct ReplayerBuilder {
    journal: PathBuf,
    from: Option<u64>,
    to: Option<u64>,
    speed: Option<f64>,
    remap: Remap,
    typed: HashMap<String, Registered>,
    services: Option<HashSet<String>>,
    late_threshold: Duration,
    spin: Duration,
    node_name: String,
    stop: Arc<AtomicBool>,
    handle_signals: bool,
    wait_for_subscribers: Option<(usize, Duration)>,
    linger: Duration,
    end_at_start: bool,
    allow_live_targets: bool,
    on_start: Option<OnStart>,
}

/// Called once a replay has started.
type OnStart = Box<dyn FnOnce() + Send>;

/// A service registered with [`ReplayerBuilder::typed`]: the type details iceoryx2 derives for its Rust types, and
/// how to open its sink.
struct Registered {
    payload: TypeInfo,
    user_header: TypeInfo,
    factory: SinkFactory,
}

impl core::fmt::Debug for ReplayerBuilder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ReplayerBuilder")
            .field("journal", &self.journal)
            .field("from", &self.from)
            .field("to", &self.to)
            .field("speed", &self.speed)
            .field("remap", &self.remap)
            .field("typed", &self.typed.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl ReplayerBuilder {
    /// Starts at this sequence number (default: the first record in the journal).
    pub fn from(mut self, seq: u64) -> Self {
        self.from = Some(seq);
        self
    }

    /// Stops after this sequence number, inclusive (default: the end of the journal, see
    /// [`ReplayerBuilder::end_at_start`]).
    pub fn to(mut self, seq: u64) -> Self {
        self.to = Some(seq);
        self
    }

    /// Sends at the recorded pace multiplied by `speed` (1.0 is the recorded pace, 2.0 twice as fast), from
    /// [`MIN_SPEED`] to [`MAX_SPEED`]. Gaps are reproduced as recorded, scaled: a pause between recording runs, or a
    /// forward step of the wall clock while recording, is waited out in real time.
    pub fn speed(mut self, speed: f64) -> Self {
        self.speed = Some(speed);
        self
    }

    /// Sends as fast as possible (the default).
    pub fn as_fast_as_possible(mut self) -> Self {
        self.speed = None;
        self
    }

    /// How recorded names map to replayed names. Default: [`Remap::default`] (prefix `replay/`).
    pub fn remap(mut self, remap: Remap) -> Self {
        self.remap = remap;
        self
    }

    /// Replays only these recorded services (default: all).
    pub fn services<I: IntoIterator<Item = S>, S: Into<String>>(mut self, names: I) -> Self {
        self.services = Some(names.into_iter().map(Into::into).collect());
        self
    }

    /// Replays the recorded service `recorded` with payload type `P` and user header type `H`, using only
    /// iceoryx2's public API. Refuses if the types don't match the recording: every descriptor of `recorded` is
    /// checked against `P` and `H` before its target is opened or reused.
    pub fn typed<P: Replayable + ?Sized, H: ReplayableHeader>(
        mut self,
        recorded: impl Into<String>,
    ) -> Self {
        let factory: SinkFactory = Box::new(|node, target, descriptor| {
            // The registration was checked against `descriptor` before (check_registration), so nothing that
            // doesn't match it creates the target.
            check_existing(node, target, descriptor)?;
            let name = service_name(target)?;
            let factory =
                P::open_service::<H>(node, &name, target, Create::Replay(descriptor.settings))?;
            let got = describe(factory.static_config(), service_version(node, &name), 0);
            if let Some(diff) = type_difference(
                descriptor,
                &got.payload,
                &got.user_header,
                None,
                AlignmentRule::ServiceAtLeastRecorded,
            ) {
                return Err(Error::Incompatible {
                    service: target.to_string(),
                    reason: format!("the registered Rust types don't match the recording: {diff}"),
                });
            }
            let (publisher, budget) = P::publisher::<H>(node, &factory, target)?;
            Ok(Box::new(TypedSink {
                factory,
                publisher,
                budget,
                target: target.to_string(),
            }))
        });
        self.typed.insert(
            recorded.into(),
            Registered {
                payload: P::payload_type(),
                user_header: header_type::<H>(),
                factory,
            },
        );
        self
    }

    /// How long before each paced send the replayer stops sleeping and spins, to absorb the operating system's
    /// timer slack. Default: 1 ms.
    pub fn spin(mut self, spin: Duration) -> Self {
        self.spin = spin;
        self
    }

    /// Sends later than this are counted as late. Default: 1 ms.
    pub fn late_threshold(mut self, threshold: Duration) -> Self {
        self.late_threshold = threshold;
        self
    }

    /// The iceoryx2 node name. Default: `tickertape-replayer`.
    pub fn node_name(mut self, name: impl Into<String>) -> Self {
        self.node_name = name.into();
        self
    }

    /// Whether this run handles `SIGINT` and `SIGTERM`. Default: true.
    ///
    /// With true, while the run is active: the first of these signals that the run's signal thread takes sets the
    /// stop flag, and the run stops cleanly (a final checkpoint for a recorder; the stop reason is "signal"). A
    /// signal it takes after that ends the process at once with exit status 130 (`SIGINT`) or 143 (`SIGTERM`), with
    /// no final checkpoint. Signals of one kind that arrive before the thread has taken the previous one are merged
    /// into one (by the signal iterator, and by the operating system, which keeps at most one pending signal of each
    /// kind), so two of the same kind sent back to back can stop the run cleanly instead of ending the process; a
    /// `SIGINT` and a `SIGTERM` are never merged.
    ///
    /// When the run returns, and no other run in the process is handling signals, `SIGINT` and `SIGTERM` end the
    /// process as their default action would: the first run installs, for the rest of the process's life, an action
    /// that emulates the default whenever no run handles signals. Handlers the process installs for these signals,
    /// before or after the first run, don't prevent that: one installed before, or later through signal-hook, still
    /// runs, and then the process ends (one installed later with `sigaction` directly replaces this action instead).
    /// An inherited "ignore" disposition (`SIGINT` in a background job of a non-interactive shell, or under `nohup`)
    /// is replaced too: after the first run, the signal ends the process. So a process that handles or ignores these
    /// signals itself should pass false and stop runs with the stop flag.
    ///
    /// With false, the run leaves the process's signal handling alone.
    pub fn handle_signals(mut self, on: bool) -> Self {
        self.handle_signals = on;
        self
    }

    /// Before the first send to each service, waits until it has at least `count` subscribers, for at most
    /// `timeout` (a timeout too large to add to the current time waits without a limit). Without this, messages sent before a subscriber connects are only delivered through the
    /// service's history, if it has one.
    pub fn wait_for_subscribers(mut self, count: usize, timeout: Duration) -> Self {
        self.wait_for_subscribers = Some((count, timeout));
        self
    }

    /// Keeps the publishers open this long after the last message (default: not at all; a duration too large to add
    /// to the current time lingers until the stop flag or a signal). A subscriber that hasn't
    /// yet taken its first sample when a publisher closes can lose what was sent to it, so a replay that ends
    /// right after a burst should linger.
    pub fn linger(mut self, linger: Duration) -> Self {
        self.linger = linger;
        self
    }

    /// Whether the replay ends at the journal's committed end as it was when the replay started (true, the default)
    /// or keeps going to the end as it is when the replayer gets there (false). With false, a replay onto names a
    /// running recorder records can feed itself without end.
    pub fn end_at_start(mut self, on: bool) -> Self {
        self.end_at_start = on;
        self
    }

    /// Allows target names that the journal is recording right now (default: refused). A recorder of such a
    /// service records the replay into the journal being replayed.
    ///
    /// While a writer holds the journal (a recorder, or an import), the replayer first reads the whole journal
    /// once, up to its committed end, and refuses every target name that a descriptor anywhere in it names, wherever
    /// that descriptor lies. Names without a descriptor yet are not known: a recorder still waiting for a service
    /// to appear records a replay that creates it. The replay then still ends at the journal's end as it was when
    /// the replay started ([`ReplayerBuilder::end_at_start`]), so it makes one pass, not a loop.
    pub fn allow_live_targets(mut self, on: bool) -> Self {
        self.allow_live_targets = on;
        self
    }

    /// Called once, after signal handling is installed and the journal is open, before the first record is read.
    pub fn on_start(mut self, f: impl FnOnce() + Send + 'static) -> Self {
        self.on_start = Some(Box::new(f));
        self
    }

    /// A flag that stops the replay when set, from any thread.
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }

    /// Checks the configuration.
    ///
    /// # Errors
    ///
    /// [`Error::Usage`] if the speed is outside [`MIN_SPEED`] to [`MAX_SPEED`] or not a number, or `to` is before
    /// `from`.
    pub fn build(self) -> Result<Replayer, Error> {
        if let Some(s) = self.speed
            && !(MIN_SPEED..=MAX_SPEED).contains(&s)
        {
            return Err(Error::Usage(format!(
                "speed must be from {MIN_SPEED} to {MAX_SPEED}, not {s}"
            )));
        }
        if let (Some(from), Some(to)) = (self.from, self.to)
            && to < from
        {
            return Err(Error::Usage(format!("to ({to}) is before from ({from})")));
        }
        Ok(Replayer { config: self })
    }
}

/// Replays a recording onto iceoryx2. See the [module docs](self).
#[derive(Debug)]
pub struct Replayer {
    config: ReplayerBuilder,
}

struct Target {
    report: ReplayServiceReport,
    sink: Box<dyn Sink>,
    /// The validated types of the descriptor that opened the target, and the recorded service and record it came
    /// from; every other descriptor resolving to this target must match exactly.
    payload: TypeInfo,
    user_header: TypeInfo,
    first: (String, u64),
}

impl Replayer {
    /// Starts configuring a replay of the journal in `journal`.
    pub fn builder(journal: impl Into<PathBuf>) -> ReplayerBuilder {
        ReplayerBuilder {
            journal: journal.into(),
            from: None,
            to: None,
            speed: None,
            remap: Remap::default(),
            typed: HashMap::new(),
            services: None,
            late_threshold: Duration::from_millis(1),
            spin: Duration::from_millis(1),
            node_name: "tickertape-replayer".into(),
            stop: Arc::new(AtomicBool::new(false)),
            handle_signals: true,
            wait_for_subscribers: None,
            linger: Duration::ZERO,
            end_at_start: true,
            allow_live_targets: false,
            on_start: None,
        }
    }

    /// A flag that stops the replay when set, from any thread.
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.config.stop)
    }

    /// Replays to the end of the journal (see [`ReplayerBuilder::end_at_start`]), `to`, a signal or the stop flag.
    /// Every recorded service must be registered with [`ReplayerBuilder::typed`]; others are refused (replay them
    /// with `Replayer::run_dynamic`).
    ///
    /// # Errors
    ///
    /// [`Error::Journal`] or [`Error::Layout`] if the recording can't be read or a record is invalid;
    /// [`Error::Incompatible`] if a target service exists with other types, or two descriptors with different types
    /// resolve to one target; [`Error::Usage`] if a service is not registered as typed, its name maps onto itself
    /// or onto a name the journal is recording right now, or a paced send time can't be represented;
    /// [`Error::Iceoryx2`] if sending fails.
    pub fn run(self) -> Result<ReplayReport, Error> {
        self.run_with(None)
    }

    pub(crate) fn run_with(self, untyped: Option<UntypedSink>) -> Result<ReplayReport, Error> {
        let mut config = self.config;
        let signals = if config.handle_signals {
            Some(Signals::register(&config.stop)?)
        } else {
            None
        };
        let signalled = signals.as_ref().map(|s| Arc::clone(&s.signalled));
        let stopped = || {
            if signalled
                .as_ref()
                .is_some_and(|s| s.load(Ordering::Relaxed))
            {
                ReplayEnd::Signal
            } else {
                ReplayEnd::StopFlag
            }
        };
        let node = NodeBuilder::new()
            .name(
                &NodeName::new(&config.node_name)
                    .map_err(|e| Error::Usage(format!("invalid node name: {e:?}")))?,
            )
            .signal_handling_mode(SignalHandlingMode::Disabled)
            .create::<ipc::Service>()
            .map_err(|e| Error::iox("create node", e))?;
        let mut reader = Reader::open(&config.journal, ReaderConfig::default())?;
        // The end now, and the names the journal is recording now (if a writer holds it, and live targets aren't
        // allowed anyway): every service with a descriptor anywhere in the journal.
        let tail = crate::tail::read(&config.journal)?;
        let live: BTreeSet<String> =
            if !config.allow_live_targets && crate::tail::has_live_writer(&config.journal)? {
                crate::catalog::service_names(&config.journal, tail.end_seq)?
            } else {
                BTreeSet::new()
            };
        let end = config.end_at_start.then_some(tail.end_seq);
        if let Some(from) = config.from {
            reader.seek(from)?;
        }
        let mut catalog = Catalog::new(&config.journal);
        let mut pacer = config
            .speed
            .map(|s| Pacer::new(s, config.spin, config.late_threshold));
        let mut targets: Vec<Target> = Vec::new();
        let mut by_descriptor: HashMap<u64, Option<usize>> = HashMap::new();
        let mut by_target: HashMap<String, usize> = HashMap::new();
        let mut report = ReplayReport {
            first_seq: None,
            last_seq: None,
            messages: 0,
            filtered: 0,
            undescribed: 0,
            other_records: 0,
            services: Vec::new(),
            pacing: None,
            end: ReplayEnd::EndOfJournal,
        };
        if let Some(f) = config.on_start.take() {
            f();
        }
        loop {
            if config.stop.load(Ordering::Relaxed) {
                report.end = stopped();
                break;
            }
            // Don't read past the end captured at the start: nothing after it is replayed.
            if end.is_some_and(|end| reader.position() >= end) {
                break;
            }
            let Some(record) = reader.try_next()? else {
                break;
            };
            let seq = record.seq;
            if end.is_some_and(|end| seq >= end) {
                break;
            }
            if config.to.is_some_and(|to| seq > to) {
                report.end = ReplayEnd::ToReached;
                break;
            }
            report.last_seq = Some(seq);
            let layout_error = |source| Error::Layout { seq, source };
            let data =
                match layout::decode(record.schema_id, record.payload).map_err(layout_error)? {
                    Decoded::Descriptor(d) => {
                        catalog.insert(seq, *d);
                        report.other_records += 1;
                        continue;
                    }
                    Decoded::Data(data) => data,
                    Decoded::Loss(loss) => {
                        // Checked against its descriptor like everywhere else, though nothing is sent for it.
                        match catalog.resolve(seq, loss.descriptor_seq)? {
                            Some(d) => {
                                loss.check(&d).map_err(layout_error)?;
                                report.other_records += 1;
                            }
                            None => report.undescribed += 1,
                        }
                        continue;
                    }
                    Decoded::Filler(_) | Decoded::Other => {
                        report.other_records += 1;
                        continue;
                    }
                };
            let Some(descriptor) = catalog.resolve(seq, data.descriptor_seq)? else {
                report.undescribed += 1;
                continue;
            };
            data.check(&descriptor).map_err(layout_error)?;
            let index = match by_descriptor.get(&data.descriptor_seq) {
                Some(&i) => i,
                None => {
                    let i = if config
                        .services
                        .as_ref()
                        .is_some_and(|s| !s.contains(&descriptor.service_name))
                    {
                        None
                    } else {
                        let target = config.remap.target(&descriptor.service_name)?;
                        if live.contains(&target) && !config.allow_live_targets {
                            return Err(Error::Usage(format!(
                                "refusing to replay \"{}\" onto \"{target}\": the journal is recording that service \
                                 right now, so the replay would be recorded into it; choose another name, stop the \
                                 recorder, or allow live targets",
                                descriptor.service_name
                            )));
                        }
                        // Every descriptor's registration is checked, also when its target is already open: a
                        // cached sink must not carry a service registered with other types, or not at all.
                        check_registration(&config, untyped.is_some(), &descriptor)?;
                        Some(match by_target.get(&target) {
                            Some(&i) => {
                                check_same_types(
                                    &targets[i],
                                    &target,
                                    &descriptor,
                                    data.descriptor_seq,
                                )?;
                                i
                            }
                            None => {
                                let sink =
                                    open_sink(&mut config, untyped, &node, &target, &descriptor)?;
                                targets.push(Target {
                                    report: ReplayServiceReport {
                                        recorded: descriptor.service_name.clone(),
                                        target: target.clone(),
                                        messages: 0,
                                    },
                                    sink,
                                    payload: descriptor.payload.clone(),
                                    user_header: descriptor.user_header.clone(),
                                    first: (descriptor.service_name.clone(), data.descriptor_seq),
                                });
                                by_target.insert(target, targets.len() - 1);
                                targets.len() - 1
                            }
                        })
                    };
                    by_descriptor.insert(data.descriptor_seq, i);
                    i
                }
            };
            let Some(index) = index else {
                report.filtered += 1;
                continue;
            };
            // Waiting for subscribers may have been cut short by a stop request: don't send after it.
            if config.stop.load(Ordering::Relaxed) {
                report.end = stopped();
                break;
            }
            let scheduled = match pacer.as_mut() {
                Some(p) => match p.wait(seq, record.local_ts_ns, &config.stop)? {
                    Some(t) => Some(t),
                    None => {
                        report.end = stopped();
                        break;
                    }
                },
                None => None,
            };
            let actual = Instant::now();
            let target = &mut targets[index];
            target.sink.send(data.user_header, data.payload)?;
            if let (Some(p), Some(t)) = (pacer.as_mut(), scheduled) {
                p.sent(seq, t, actual);
            }
            target.report.messages += 1;
            report.messages += 1;
            report.first_seq.get_or_insert(seq);
        }
        if !targets.is_empty() && !config.linger.is_zero() {
            let until = crate::recorder::deadline_after(config.linger);
            while until.is_none_or(|u| Instant::now() < u) && !config.stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(5).min(config.linger));
            }
        }
        drop(signals);
        report.services = targets.into_iter().map(|t| t.report).collect();
        report.pacing = pacer.map(|p| p.report);
        Ok(report)
    }
}

/// Requires a descriptor that resolves to an existing target to have exactly the target's types.
fn check_same_types(
    existing: &Target,
    target: &str,
    descriptor: &Descriptor,
    descriptor_seq: u64,
) -> Result<(), Error> {
    if existing.payload == descriptor.payload && existing.user_header == descriptor.user_header {
        return Ok(());
    }
    let show = |t: &TypeInfo| {
        format!(
            "{:?} {} (size {}, alignment {})",
            t.variant, t.name, t.size, t.alignment
        )
    };
    Err(Error::Incompatible {
        service: target.to_string(),
        reason: format!(
            "recorded service \"{}\" (descriptor {descriptor_seq}) has payload {} and user header {}, but the target \
             already carries payload {} and user header {} from recorded service \"{}\" (descriptor {}); replay \
             them onto different names (--map) or separately (--from/--to)",
            descriptor.service_name,
            show(&descriptor.payload),
            show(&descriptor.user_header),
            show(&existing.payload),
            show(&existing.user_header),
            existing.first.0,
            existing.first.1,
        ),
    })
}

/// Checks a recorded service's registration before its target is opened or reused: a service registered with
/// [`ReplayerBuilder::typed`] must have the registered types; an unregistered one needs an untyped sink
/// (`Replayer::run_dynamic`).
fn check_registration(
    config: &ReplayerBuilder,
    untyped: bool,
    descriptor: &Descriptor,
) -> Result<(), Error> {
    match config.typed.get(&descriptor.service_name) {
        Some(r) => match type_difference(
            descriptor,
            &r.payload,
            &r.user_header,
            None,
            AlignmentRule::RecordedAtLeastRegistered,
        ) {
            None => Ok(()),
            Some(diff) => Err(Error::Incompatible {
                service: descriptor.service_name.clone(),
                reason: format!("the registered Rust types don't match the recording: {diff}"),
            }),
        },
        None if untyped => Ok(()),
        None => Err(Error::Usage(format!(
            "service \"{}\" (payload type {}) is not registered with ReplayerBuilder::typed; replaying services \
             by their recorded type details needs Replayer::run_dynamic (feature `dynamic`)",
            descriptor.service_name, descriptor.payload.name
        ))),
    }
}

fn open_sink(
    config: &mut ReplayerBuilder,
    untyped: Option<UntypedSink>,
    node: &Node<ipc::Service>,
    target: &str,
    descriptor: &Descriptor,
) -> Result<Box<dyn Sink>, Error> {
    let sink = match (config.typed.get_mut(&descriptor.service_name), untyped) {
        (Some(registered), _) => (registered.factory)(node, target, descriptor)?,
        (None, Some(untyped)) => {
            check_existing(node, target, descriptor)?;
            untyped(node, target, descriptor)?
        }
        (None, None) => {
            return Err(Error::Usage(format!(
                "service \"{}\" (payload type {}) is not registered with ReplayerBuilder::typed; replaying services \
                 by their recorded type details needs Replayer::run_dynamic (feature `dynamic`)",
                descriptor.service_name, descriptor.payload.name
            )));
        }
    };
    if let Some((count, timeout)) = config.wait_for_subscribers {
        let deadline = crate::recorder::deadline_after(timeout);
        while sink.subscribers() < count {
            if config.stop.load(Ordering::Relaxed) {
                break;
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Err(Error::Usage(format!(
                    "service \"{target}\" has {} subscribers after {timeout:?}; waited for {count}",
                    sink.subscribers()
                )));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    Ok(sink)
}

/// Refuses clearly if `target` already exists with types other than the recorded ones.
pub(crate) fn check_existing(
    node: &Node<ipc::Service>,
    target: &str,
    descriptor: &Descriptor,
) -> Result<(), Error> {
    let name = service_name(target)?;
    let details = ipc::Service::details(&name, node.config(), MessagingPattern::PublishSubscribe)
        .map_err(|e| Error::iox(&format!("read details of \"{target}\""), e))?;
    let Some(details) = details else {
        return Ok(());
    };
    let got = describe(
        details.static_details.publish_subscribe(),
        service_version(node, &name),
        0,
    );
    match type_difference(
        descriptor,
        &got.payload,
        &got.user_header,
        Some(&got.system_header),
        AlignmentRule::ServiceAtLeastRecorded,
    ) {
        None => Ok(()),
        Some(diff) => Err(Error::Incompatible {
            service: target.to_string(),
            reason: format!("it already exists with other types: {diff}"),
        }),
    }
}

/// Which alignments [`type_difference`] accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AlignmentRule {
    /// A service (existing, or as opened) needs at least the recorded alignment: iceoryx2 places the recorded
    /// values at that alignment.
    ServiceAtLeastRecorded,
    /// A registered Rust type accepts a recorded alignment that is a power of two not below its own: iceoryx2's
    /// `payload_alignment` raises a service's payload alignment above the type's, and the type's values are valid
    /// at any multiple of its alignment.
    RecordedAtLeastRegistered,
}

/// Describes how the service's (or registered) types differ from the recorded ones, or `None` if they are
/// compatible: same variant, name and size, and alignments as `rule` says.
fn type_difference(
    recorded: &Descriptor,
    payload: &TypeInfo,
    user_header: &TypeInfo,
    system_header: Option<&TypeInfo>,
    rule: AlignmentRule,
) -> Option<String> {
    let mut parts = Vec::new();
    let mut compare = |what: &str, want: &TypeInfo, got: &TypeInfo| {
        let alignment_ok = match rule {
            AlignmentRule::ServiceAtLeastRecorded => got.alignment >= want.alignment,
            AlignmentRule::RecordedAtLeastRegistered => {
                want.alignment.is_power_of_two() && want.alignment >= got.alignment
            }
        };
        if want.variant != got.variant
            || want.name != got.name
            || want.size != got.size
            || !alignment_ok
        {
            parts.push(format!(
                "{what} is {:?} {} (size {}, alignment {}), recorded {:?} {} (size {}, alignment {})",
                got.variant, got.name, got.size, got.alignment, want.variant, want.name, want.size, want.alignment
            ));
        }
    };
    compare("payload", &recorded.payload, payload);
    compare("user header", &recorded.user_header, user_header);
    if let Some(h) = system_header
        && h.size != recorded.system_header.size
    {
        parts.push(format!(
            "iceoryx2's sample header has {} bytes, recorded {} (different iceoryx2 versions?)",
            h.size, recorded.system_header.size
        ));
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

struct TypedSink<P: Replayable + ?Sized, H: ReplayableHeader> {
    factory: iceoryx2::service::port_factory::publish_subscribe::PortFactory<ipc::Service, P, H>,
    publisher: Publisher<ipc::Service, P, H>,
    budget: PortBudget,
    target: String,
}

impl<P: Replayable + ?Sized, H: ReplayableHeader> Sink for TypedSink<P, H> {
    fn send(&mut self, user_header: &[u8], payload: &[u8]) -> Result<(), Error> {
        self.budget.check_payload(&self.target, payload.len())?;
        P::send(&self.publisher, user_header, payload)
    }

    fn subscribers(&self) -> usize {
        self.factory.dynamic_config().number_of_subscribers()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_remap_prefixes_names() {
        assert_eq!(Remap::default().target("a/b").unwrap(), "replay/a/b");
        assert_eq!(Remap::default().prefix("x.").target("a").unwrap(), "x.a");
    }

    #[test]
    fn original_names_need_an_explicit_request() {
        assert!(matches!(
            Remap::default().prefix("").target("a"),
            Err(Error::Usage(_))
        ));
        assert!(matches!(
            Remap::default().rule("a", "a").target("a"),
            Err(Error::Usage(_))
        ));
        assert_eq!(Remap::default().original_names().target("a").unwrap(), "a");
        assert_eq!(Remap::default().rule("a", "b").target("a").unwrap(), "b");
        assert_eq!(
            Remap::default()
                .original_names()
                .rule("a", "a")
                .target("a")
                .unwrap(),
            "a"
        );
    }

    fn fixed(name: &str, size: u64, alignment: u64) -> TypeInfo {
        TypeInfo {
            variant: crate::layout::TypeVariant::FixedSize,
            name: name.into(),
            size,
            alignment,
        }
    }

    fn recorded(payload: TypeInfo) -> Descriptor {
        Descriptor {
            run_start_seq: 0,
            run_start_unix_ns: 0,
            imported: false,
            iceoryx2_version: crate::layout::Version {
                major: 0,
                minor: 10,
                patch: 0,
            },
            settings: crate::layout::ServiceSettings {
                history_size: 0,
                subscriber_max_buffer_size: 2,
                subscriber_max_borrowed_samples: 2,
                max_publishers: 2,
                max_subscribers: 8,
                max_nodes: 20,
                safe_overflow: true,
            },
            recorder_buffer_size: 0,
            counter: None,
            payload,
            user_header: fixed("()", 0, 1),
            system_header: fixed("Header", 48, 8),
            service_name: "s".into(),
            recorder: String::new(),
        }
    }

    /// Review round 4: a registered Rust type accepts a recorded payload alignment raised above its own (a
    /// power of two), and nothing else that differs; a service (existing or opened) needs at least the recorded
    /// alignment.
    #[test]
    fn alignment_rules() {
        use AlignmentRule::*;
        let header = fixed("()", 0, 1);
        let differs = |recorded_payload: TypeInfo, other: TypeInfo, rule| {
            type_difference(&recorded(recorded_payload), &other, &header, None, rule)
        };
        let bytes = |alignment| fixed("[u8; 64]", 64, alignment);
        // Registered `[u8; 64]` (alignment 1), recorded with alignment 64: accepted (refused in round 3).
        assert_eq!(
            differs(bytes(64), bytes(1), RecordedAtLeastRegistered),
            None
        );
        assert_eq!(differs(bytes(1), bytes(1), RecordedAtLeastRegistered), None);
        // A recorded alignment below the registered type's, or not a power of two: refused.
        let u64_type = |alignment| fixed("u64", 8, alignment);
        assert!(differs(u64_type(4), u64_type(8), RecordedAtLeastRegistered).is_some());
        assert!(differs(bytes(48), bytes(1), RecordedAtLeastRegistered).is_some());
        // Name and size still have to match.
        assert!(
            differs(
                bytes(64),
                fixed("[i8; 64]", 64, 1),
                RecordedAtLeastRegistered
            )
            .is_some()
        );
        assert!(
            differs(
                bytes(64),
                fixed("[u8; 64]", 63, 1),
                RecordedAtLeastRegistered
            )
            .is_some()
        );
        // A service: at least the recorded alignment.
        assert_eq!(differs(bytes(64), bytes(64), ServiceAtLeastRecorded), None);
        assert_eq!(differs(bytes(64), bytes(128), ServiceAtLeastRecorded), None);
        assert!(differs(bytes(64), bytes(1), ServiceAtLeastRecorded).is_some());
    }
}
