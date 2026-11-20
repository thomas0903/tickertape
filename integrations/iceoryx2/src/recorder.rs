//! Record iceoryx2 publish/subscribe services into a tickertape journal.
//!
//! [`Recorder`] subscribes to each configured service and appends one tickertape record per message (see
//! [`layout`] for the bytes). It is the journal's only writer while it runs.
//!
//! # Example
//!
//! ```no_run
//! use iceoryx2::prelude::ZeroCopySend;
//! use tickertape_iceoryx2::recorder::Recorder;
//! use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
//!
//! #[derive(Debug, Default, Clone, Copy, ZeroCopySend, IntoBytes, FromBytes, Immutable, KnownLayout)]
//! #[repr(C)]
//! struct Tick {
//!     seq: u64,
//!     price: i64,
//! }
//!
//! # fn main() -> Result<(), tickertape_iceoryx2::Error> {
//! let report = Recorder::builder("/tmp/ticks")
//!     .service_typed::<Tick>("market/ticks")
//!     .build()?
//!     .run()?; // until SIGINT or SIGTERM
//! println!("{} messages recorded", report.messages);
//! # Ok(())
//! # }
//! ```
//!
//! # Waiting for messages
//!
//! iceoryx2 subscribers can't wake a WaitSet by themselves: only event-service listeners can. The recorder waits on
//! an iceoryx2 [`WaitSet`] with an interval attachment (the poll interval, 1 ms by default) and drains every
//! subscriber when it wakes. If the publishers also notify an event service after sending, name it with
//! [`TypedService::notify_service`] and the recorder wakes on each notification as well. Either way it sleeps
//! between wake-ups; it never spins. Each pass takes at most 4,096 messages from each service, then looks at the
//! checkpoint timer and the stop conditions; while a service has more, the next pass starts without waiting, so one
//! busy service can't starve the others.
//!
//! # Buffer size, overflow and loss
//!
//! Between wake-ups, messages wait in the recorder's subscriber buffer, whose size is the smaller of
//! [`RecorderBuilder::buffer_size`] (1,024 by default) and the service's `subscriber_max_buffer_size`. iceoryx2's
//! default for the latter is 2. If the recorder creates a service, it creates it with `subscriber_max_buffer_size`
//! equal to its buffer size; if a publisher created it first, the publisher's setting wins. When the buffer is
//! full, a service with safe overflow (iceoryx2's default) replaces the oldest unread sample, which the recorder
//! never sees; without safe overflow, the publisher's backpressure strategy decides (block, retry or discard).
//! The periodic checkpoints run in the background (`tickertape::Writer::request_checkpoint`, ADR 0038), so recording
//! continues while one syncs to disk; only the final checkpoint at a stop blocks the recording thread, after the last
//! message is taken. The descriptor records both the service's settings and
//! the buffer size the recorder got. To detect loss, give each publisher a counter and configure a
//! [`CounterField`]: each gap becomes a [`crate::layout::LossRecord`].
//!
//! The recorder trusts a publisher's element count for a slice payload, as any iceoryx2 subscriber does.
//!
//! # Services that fail
//!
//! A service that can't be opened yet (a runtime-typed service whose publisher hasn't started) is retried every
//! 100 ms. So is a service that fails: its types or settings are outside what the [layout](crate::layout#validity-rules)
//! accepts, it exists with other types, or receiving from it fails. The failure is reported for that service
//! ([`ServiceReport::errors`], [`ServiceReport::last_error`], [`RecorderBuilder::on_error`]) and the other services
//! keep recording. Journal errors stop everything: on a full volume, or a message too large for a journal record
//! (larger than a segment), the recorder takes a final checkpoint of what it has and returns the error.
//!
//! While the recorder runs it keeps each service it records alive, with the types it opened it with: a publisher
//! restarted with different types is refused by iceoryx2, and the recorder's types win until it stops.
//!
//! # Recording runs, restarts and durability
//!
//! Each run of the recorder is a recording run. It writes a descriptor for each service before that service's first
//! message, and again so that every segment holds the descriptors of the records in it: each new segment starts with
//! the descriptors of all open services, after a filler record that ends the previous segment when one is needed
//! (the rules, boundaries and the one exception, a record too large to share a segment with its descriptor, are under
//! [where descriptors lie](crate::layout#where-descriptors-lie)). So segments left after retention or archiving can
//! still be decoded. A recorder restarted on the same journal continues it after recovery; tickertape sequence
//! numbers stay gap-free. If the service keeps history (`history_size > 0`), a reconnecting recorder receives that
//! history again; such repeats are recorded as messages again, and with a counter configured they are not reported
//! as losses (counters are tracked per publisher within one run, and a new run starts afresh). Records are visible
//! to readers as soon as they are written and durable after a checkpoint: every
//! [`RecorderBuilder::checkpoint_interval`] (1 s by default) the recorder requests a background checkpoint of what it
//! has recorded, which becomes durable when that checkpoint finishes (a request made while the previous one still
//! runs waits for it, and is joined by later ones), and on a clean stop (`SIGINT`, `SIGTERM`, a stop flag, or a limit)
//! it takes a blocking checkpoint, which first waits for the background one. A background checkpoint that fails is
//! reported as [`Error::Journal`] by the next periodic request (or by the final checkpoint, whichever comes first),
//! and stops the recorder as any journal error does.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use iceoryx2::port::listener::Listener;
use iceoryx2::port::subscriber::Subscriber;
use iceoryx2::prelude::*;
use iceoryx2::service::builder::publish_subscribe::{
    PublishSubscribeCreateError, PublishSubscribeOpenError,
};
use iceoryx2::service::static_config::message_type_details::{
    TypeDetail, TypeVariant as IoxVariant,
};
use iceoryx2::service::static_config::publish_subscribe::StaticConfig as PubSubConfig;
use serde::Serialize;
use tickertape::{NewRecord, WriterConfig};

use crate::Error;
use crate::layout::{
    self, CounterField, DataRecord, Descriptor, LossKind, LossRecord, MAX_BUFFER_SIZE, SCHEMA_DATA,
    SCHEMA_LOSS, ServiceSettings, TypeInfo, TypeVariant, Version,
};
use crate::placement::Journal;
use crate::typed::{Create, Recordable, RecordableHeader};

/// One received message, as the recorder sees it.
pub(crate) struct Message<'a> {
    pub user_header: &'a [u8],
    pub payload: &'a [u8],
    pub origin: u128,
    pub venue_ts_ns: u64,
}

/// What a source knows about its service once open.
pub(crate) struct Described {
    pub payload: TypeInfo,
    pub user_header: TypeInfo,
    pub system_header: TypeInfo,
    pub settings: ServiceSettings,
    pub iceoryx2_version: Version,
    pub buffer_size: usize,
}

/// An open subscription.
pub(crate) trait Source {
    fn described(&self) -> &Described;
    fn listener(&self) -> Option<&Listener<ipc::Service>>;
    fn drain(
        &self,
        max: usize,
        f: &mut dyn FnMut(Message<'_>) -> Result<(), Error>,
    ) -> Result<usize, Error>;
}

/// The result of trying to open a configured service.
#[cfg_attr(not(feature = "dynamic"), allow(dead_code))]
pub(crate) enum Opened {
    /// Open, with a subscriber.
    Ready(Box<dyn Source>),
    /// It doesn't exist yet (a runtime-typed service whose publisher hasn't started); try again later.
    NotYet,
}

/// A configured service that may not exist yet.
pub(crate) trait SourceFactory: Send {
    fn name(&self) -> &str;
    fn counter(&self) -> Option<CounterField>;
    fn open(&mut self, node: &Node<ipc::Service>, buffer_size: usize) -> Result<Opened, Error>;
}

/// Reads `venue_ts_ns` from a typed user header.
type VenueTs<H> = Arc<dyn Fn(&H) -> u64 + Send + Sync>;
/// Called when a service is opened.
type OnOpen = Box<dyn FnMut(&str, u64) + Send>;
/// Called when a service fails.
type OnError = Box<dyn FnMut(&str, &Error) + Send>;
/// Called once a recording run has started.
type OnStart = Box<dyn FnOnce(u64) + Send>;

/// A service whose payload type `P` (a type or a slice) and user header type `H` are known at compile time.
///
/// Uses only iceoryx2's public API. See [`Recordable`] for the requirements on `P`.
pub struct TypedService<P: ?Sized, H = ()> {
    name: String,
    venue_ts: Option<VenueTs<H>>,
    counter: Option<CounterField>,
    notify: Option<String>,
    _payload: PhantomData<fn() -> Box<P>>,
}

impl<P: ?Sized, H> core::fmt::Debug for TypedService<P, H> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TypedService")
            .field("name", &self.name)
            .field("venue_ts", &self.venue_ts.is_some())
            .field("counter", &self.counter)
            .field("notify", &self.notify)
            .finish()
    }
}

impl<P: Recordable + ?Sized, H: RecordableHeader> TypedService<P, H> {
    /// A service named `name`.
    pub fn new(name: impl Into<String>) -> Self {
        TypedService {
            name: name.into(),
            venue_ts: None,
            counter: None,
            notify: None,
            _payload: PhantomData,
        }
    }

    /// Reads each record's `venue_ts_ns` from the user header (otherwise 0).
    pub fn venue_ts(mut self, f: impl Fn(&H) -> u64 + Send + Sync + 'static) -> Self {
        self.venue_ts = Some(Arc::new(f));
        self
    }

    /// Checks a publisher-side counter and writes a loss record for each gap.
    pub fn counter(mut self, counter: CounterField) -> Self {
        self.counter = Some(counter);
        self
    }

    /// Also wakes up when a notifier signals the event service `name` (created if missing).
    pub fn notify_service(mut self, name: impl Into<String>) -> Self {
        self.notify = Some(name.into());
        self
    }
}

impl<P: Recordable + ?Sized, H: RecordableHeader> SourceFactory for TypedService<P, H> {
    fn name(&self) -> &str {
        &self.name
    }

    fn counter(&self) -> Option<CounterField> {
        self.counter
    }

    fn open(&mut self, node: &Node<ipc::Service>, buffer_size: usize) -> Result<Opened, Error> {
        let name = service_name(&self.name)?;
        let service = P::open_service::<H>(
            node,
            &name,
            &self.name,
            Create::Recorder {
                buffer: buffer_size,
            },
        )?;
        let cfg = service.static_config();
        let buffer = buffer_size.min(cfg.subscriber_max_buffer_size()).max(1);
        crate::typed::check_subscriber(node, &self.name, cfg, buffer, service.dynamic_config())?;
        let subscriber = service
            .subscriber_builder()
            .buffer_size(buffer)
            .create()
            .map_err(|e| Error::iox("create subscriber", e))?;
        let described = describe(cfg, service_version(node, &name), buffer);
        let listener = open_listener(node, self.notify.as_deref())?;
        Ok(Opened::Ready(Box::new(TypedSource {
            subscriber,
            venue_ts: self.venue_ts.clone(),
            described,
            listener,
        })))
    }
}

struct TypedSource<P: Recordable + ?Sized, H: RecordableHeader> {
    subscriber: Subscriber<ipc::Service, P, H>,
    venue_ts: Option<VenueTs<H>>,
    described: Described,
    listener: Option<Listener<ipc::Service>>,
}

impl<P: Recordable + ?Sized, H: RecordableHeader> Source for TypedSource<P, H> {
    fn described(&self) -> &Described {
        &self.described
    }

    fn listener(&self) -> Option<&Listener<ipc::Service>> {
        self.listener.as_ref()
    }

    fn drain(
        &self,
        max: usize,
        f: &mut dyn FnMut(Message<'_>) -> Result<(), Error>,
    ) -> Result<usize, Error> {
        P::drain(&self.subscriber, max, &mut |header, payload, origin| {
            f(Message {
                user_header: header.as_bytes(),
                payload,
                origin,
                venue_ts_ns: self.venue_ts.as_ref().map_or(0, |v| v(header)),
            })
        })
    }
}

/// The instant `d` from now, or `None` if that can't be represented (`Duration::MAX`): no deadline.
pub(crate) fn deadline_after(d: Duration) -> Option<Instant> {
    Instant::now().checked_add(d)
}

pub(crate) fn service_name(name: &str) -> Result<ServiceName, Error> {
    ServiceName::new(name)
        .map_err(|e| Error::Usage(format!("invalid iceoryx2 service name \"{name}\": {e:?}")))
}

/// Maps an error opening a publish-subscribe service: types, settings or messaging pattern that don't match are
/// [`Error::Incompatible`]; everything else is [`Error::Iceoryx2`].
pub(crate) fn open_error(name: &str, e: PublishSubscribeOpenError) -> Error {
    use PublishSubscribeOpenError as E;
    match e {
        E::IncompatibleTypes
        | E::IncompatibleMessagingPattern
        | E::IncompatibleAttributes
        | E::IncompatibleOverflowBehavior
        | E::DoesNotSupportRequestedMinBufferSize
        | E::DoesNotSupportRequestedMinHistorySize
        | E::DoesNotSupportRequestedMinSubscriberBorrowedSamples
        | E::DoesNotSupportRequestedAmountOfPublishers
        | E::DoesNotSupportRequestedAmountOfSubscribers
        | E::DoesNotSupportRequestedAmountOfNodes => Error::Incompatible {
            service: name.to_string(),
            reason: format!("iceoryx2 refused to open it ({e:?})"),
        },
        _ => Error::iox(&format!("open service \"{name}\""), e),
    }
}

/// True if an open error means the service isn't there (yet, or any more): try again later.
#[cfg_attr(not(feature = "dynamic"), allow(dead_code))]
pub(crate) fn not_there_yet(e: &PublishSubscribeOpenError) -> bool {
    matches!(
        e,
        PublishSubscribeOpenError::DoesNotExist
            | PublishSubscribeOpenError::IsMarkedForDestruction
            | PublishSubscribeOpenError::HangsInCreation
    )
}

/// Maps an error creating a publish-subscribe service.
pub(crate) fn create_error(name: &str, e: PublishSubscribeCreateError) -> Error {
    Error::iox(&format!("create service \"{name}\""), e)
}

pub(crate) fn open_listener(
    node: &Node<ipc::Service>,
    name: Option<&str>,
) -> Result<Option<Listener<ipc::Service>>, Error> {
    let Some(name) = name else { return Ok(None) };
    let event = node
        .service_builder(&service_name(name)?)
        .event()
        .open_or_create()
        .map_err(|e| Error::iox(&format!("open event service \"{name}\""), e))?;
    let listener = event
        .listener_builder()
        .create()
        .map_err(|e| Error::iox("create listener", e))?;
    Ok(Some(listener))
}

pub(crate) fn type_info(d: &TypeDetail) -> TypeInfo {
    TypeInfo {
        variant: match d.variant() {
            IoxVariant::FixedSize => TypeVariant::FixedSize,
            IoxVariant::Dynamic => TypeVariant::Dynamic,
        },
        name: d.type_name().to_string(),
        size: d.size() as u64,
        alignment: d.alignment() as u64,
    }
}

pub(crate) fn describe(
    cfg: &PubSubConfig,
    iceoryx2_version: Version,
    buffer_size: usize,
) -> Described {
    let types = cfg.message_type_details();
    Described {
        payload: type_info(&types.payload),
        user_header: type_info(&types.user_header),
        system_header: type_info(&types.header),
        settings: ServiceSettings {
            history_size: cfg.history_size() as u64,
            subscriber_max_buffer_size: cfg.subscriber_max_buffer_size() as u64,
            subscriber_max_borrowed_samples: cfg.subscriber_max_borrowed_samples() as u64,
            max_publishers: cfg.max_publishers() as u64,
            max_subscribers: cfg.max_subscribers() as u64,
            max_nodes: cfg.max_nodes() as u64,
            safe_overflow: cfg.has_safe_overflow(),
        },
        iceoryx2_version,
        buffer_size,
    }
}

/// The iceoryx2 version that created the service, or 0.0.0 if it can't be read.
pub(crate) fn service_version(node: &Node<ipc::Service>, name: &ServiceName) -> Version {
    match ipc::Service::details(name, node.config(), MessagingPattern::PublishSubscribe) {
        Ok(Some(details)) => {
            let v = details.static_details.iceoryx2_version();
            Version {
                major: v.major(),
                minor: v.minor(),
                patch: v.patch(),
            }
        }
        _ => Version {
            major: 0,
            minor: 0,
            patch: 0,
        },
    }
}

/// Why [`Recorder::run`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StopReason {
    /// `SIGINT` or `SIGTERM`, during this run.
    Signal,
    /// The stop flag was set.
    StopFlag,
    /// [`RecorderBuilder::max_messages`] were recorded.
    MaxMessages,
    /// [`RecorderBuilder::max_duration`] passed.
    MaxDuration,
}

/// Per-service results of a recording run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServiceReport {
    /// Service name.
    pub name: String,
    /// Sequence number of this run's first descriptor of the service, if the service was opened.
    pub descriptor_seq: Option<u64>,
    /// Descriptor records written for the service: the first, one per new segment, and one per reopening.
    pub descriptors: u64,
    /// Messages recorded.
    pub messages: u64,
    /// Payload bytes recorded.
    pub payload_bytes: u64,
    /// The recorder subscriber's buffer size, if opened.
    pub buffer_size: Option<usize>,
    /// The service's `subscriber_max_buffer_size`, if opened.
    pub subscriber_max_buffer_size: Option<u64>,
    /// Whether the service drops the oldest sample when a subscriber buffer is full, if opened.
    pub safe_overflow: Option<bool>,
    /// Counter gaps found (needs a [`CounterField`]).
    pub gaps: u64,
    /// Messages missing according to those gaps (saturating).
    pub missing: u64,
    /// Counter regressions found (repeats, restarts).
    pub backwards: u64,
    /// Failures to open or receive from the service; each is retried.
    pub errors: u64,
    /// The latest failure, if any.
    pub last_error: Option<String>,
}

/// The results of [`Recorder::run`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecorderReport {
    /// This recording run's `run_start_seq`: the journal's next sequence number when the recorder opened it.
    pub run_start_seq: u64,
    /// The journal's next sequence number when the recorder stopped.
    pub next_seq: u64,
    /// Messages recorded, all services.
    pub messages: u64,
    /// Checkpoints: the background checkpoints requested every [`RecorderBuilder::checkpoint_interval`] (requests
    /// made while an earlier one had not started share its checkpoint), plus the final blocking one.
    pub checkpoints: u64,
    /// Filler records written: each ends a segment early so that the descriptors start the next one (see
    /// [where descriptors lie](crate::layout#where-descriptors-lie)).
    pub fillers: u64,
    /// Why it stopped.
    pub stop_reason: StopReason,
    /// Per service.
    pub services: Vec<ServiceReport>,
}

/// Configures a [`Recorder`]. Start with [`Recorder::builder`].
pub struct RecorderBuilder {
    journal: PathBuf,
    writer_config: WriterConfig,
    checkpoint_interval: Duration,
    poll_interval: Duration,
    retry_interval: Duration,
    buffer_size: usize,
    node_name: String,
    services: Vec<Box<dyn SourceFactory>>,
    stop: Arc<AtomicBool>,
    handle_signals: bool,
    max_messages: Option<u64>,
    max_duration: Option<Duration>,
    on_open: Option<OnOpen>,
    on_error: Option<OnError>,
    on_start: Option<OnStart>,
}

impl core::fmt::Debug for RecorderBuilder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RecorderBuilder")
            .field("journal", &self.journal)
            .field(
                "services",
                &self
                    .services
                    .iter()
                    .map(|s| s.name().to_string())
                    .collect::<Vec<_>>(),
            )
            .field("checkpoint_interval", &self.checkpoint_interval)
            .field("poll_interval", &self.poll_interval)
            .field("buffer_size", &self.buffer_size)
            .finish_non_exhaustive()
    }
}

impl RecorderBuilder {
    /// Records the service `name`, whose payload type is `P` (a type or a slice) and which has no user header.
    pub fn service_typed<P: Recordable + ?Sized>(self, name: impl Into<String>) -> Self {
        self.typed(TypedService::<P, ()>::new(name))
    }

    /// Records a typed service with a user header and options.
    pub fn typed<P: Recordable + ?Sized, H: RecordableHeader>(
        mut self,
        service: TypedService<P, H>,
    ) -> Self {
        self.services.push(Box::new(service));
        self
    }

    /// Records a service whose types are read from the running service (feature `dynamic`).
    #[cfg(feature = "dynamic")]
    pub fn dynamic(mut self, service: crate::dynamic::DynamicService) -> Self {
        self.services.push(Box::new(service));
        self
    }

    /// Settings for the journal writer. Default: [`WriterConfig::default`].
    pub fn writer_config(mut self, config: WriterConfig) -> Self {
        self.writer_config = config;
        self
    }

    /// How often to request a background checkpoint, making recorded messages durable without pausing recording.
    /// Default: 1 s.
    pub fn checkpoint_interval(mut self, interval: Duration) -> Self {
        self.checkpoint_interval = interval;
        self
    }

    /// The WaitSet's interval: the longest a message waits in the subscriber buffer when no event wakes the
    /// recorder earlier. Default: 1 ms.
    pub fn poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// How often a service that isn't open yet, or failed, is tried again. Default: 100 ms.
    pub fn retry_interval(mut self, interval: Duration) -> Self {
        self.retry_interval = interval;
        self
    }

    /// The subscriber buffer the recorder asks for, capped by the service's `subscriber_max_buffer_size`.
    /// Default: 1,024; at most [`MAX_BUFFER_SIZE`].
    pub fn buffer_size(mut self, size: usize) -> Self {
        self.buffer_size = size;
        self
    }

    /// The iceoryx2 node name. Default: `tickertape-recorder`.
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

    /// Stops after this many messages, all services.
    pub fn max_messages(mut self, n: u64) -> Self {
        self.max_messages = Some(n);
        self
    }

    /// Stops after this long.
    pub fn max_duration(mut self, d: Duration) -> Self {
        self.max_duration = Some(d);
        self
    }

    /// Called with the service name and descriptor sequence number when a service is opened and its first
    /// descriptor written.
    pub fn on_open(mut self, f: impl FnMut(&str, u64) + Send + 'static) -> Self {
        self.on_open = Some(Box::new(f));
        self
    }

    /// Called with the service name and the error when a service fails to open, or receiving from it fails. The
    /// service is retried; the other services keep recording.
    pub fn on_error(mut self, f: impl FnMut(&str, &Error) + Send + 'static) -> Self {
        self.on_error = Some(Box::new(f));
        self
    }

    /// Called once with the run's `run_start_seq`, after signal handling is installed and the journal is open,
    /// before any service is opened.
    pub fn on_start(mut self, f: impl FnOnce(u64) + Send + 'static) -> Self {
        self.on_start = Some(Box::new(f));
        self
    }

    /// A flag that stops the recorder cleanly when set, from any thread.
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }

    /// Checks the configuration.
    ///
    /// # Errors
    ///
    /// [`Error::Usage`] if there are no services, a name is repeated or invalid, an interval or the buffer size is
    /// zero, or the buffer size is larger than [`MAX_BUFFER_SIZE`].
    pub fn build(self) -> Result<Recorder, Error> {
        if self.services.is_empty() {
            return Err(Error::Usage("no services to record".into()));
        }
        let mut names: Vec<&str> = self.services.iter().map(|s| s.name()).collect();
        names.sort_unstable();
        if let Some(w) = names.windows(2).find(|w| w[0] == w[1]) {
            return Err(Error::Usage(format!(
                "service \"{}\" is listed twice",
                w[0]
            )));
        }
        if self.checkpoint_interval.is_zero()
            || self.poll_interval.is_zero()
            || self.retry_interval.is_zero()
            || self.buffer_size == 0
        {
            return Err(Error::Usage(
                "the checkpoint, poll and retry intervals and the buffer size must be positive"
                    .into(),
            ));
        }
        if self.buffer_size as u64 > MAX_BUFFER_SIZE {
            return Err(Error::Usage(format!(
                "the buffer size {} is larger than the limit of {MAX_BUFFER_SIZE}",
                self.buffer_size
            )));
        }
        for s in &self.services {
            service_name(s.name())?;
        }
        Ok(Recorder { config: self })
    }
}

/// Records iceoryx2 services into a tickertape journal. See the [module docs](self).
#[derive(Debug)]
pub struct Recorder {
    config: RecorderBuilder,
}

/// Per-service bookkeeping for an open service, kept apart from the sources so the WaitSet can borrow those while
/// this changes.
struct Tracking {
    descriptor: Arc<Descriptor>,
    counter: Option<CounterField>,
    next_counter: HashMap<u128, u64>,
}

impl Recorder {
    /// Starts configuring a recorder that writes to the journal in `journal`.
    pub fn builder(journal: impl Into<PathBuf>) -> RecorderBuilder {
        RecorderBuilder {
            journal: journal.into(),
            writer_config: WriterConfig::default(),
            checkpoint_interval: Duration::from_secs(1),
            poll_interval: Duration::from_millis(1),
            retry_interval: Duration::from_millis(100),
            buffer_size: 1024,
            node_name: "tickertape-recorder".into(),
            services: Vec::new(),
            stop: Arc::new(AtomicBool::new(false)),
            handle_signals: true,
            max_messages: None,
            max_duration: None,
            on_open: None,
            on_error: None,
            on_start: None,
        }
    }

    /// A flag that stops the recorder cleanly when set, from any thread.
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.config.stop)
    }

    /// Opens the journal and records until a signal, the stop flag or a limit. Services that can't be opened yet,
    /// or fail, are retried (see the [module docs](self#services-that-fail)).
    ///
    /// # Errors
    ///
    /// [`Error::Journal`] if the journal can't be opened or written (a final checkpoint is attempted before
    /// returning an append error); [`Error::Iceoryx2`] if the iceoryx2 node or WaitSet can't be created.
    pub fn run(self) -> Result<RecorderReport, Error> {
        let mut config = self.config;
        let signals = if config.handle_signals {
            Some(Signals::register(&config.stop)?)
        } else {
            None
        };
        let node = NodeBuilder::new()
            .name(
                &NodeName::new(&config.node_name)
                    .map_err(|e| Error::Usage(format!("invalid node name: {e:?}")))?,
            )
            .signal_handling_mode(SignalHandlingMode::Disabled)
            .create::<ipc::Service>()
            .map_err(|e| Error::iox("create node", e))?;
        let mut journal = Journal::open(
            &config.journal,
            config.writer_config.clone(),
            config.services.len(),
        )?;
        let run = RunInfo {
            start_seq: journal.writer.next_seq(),
            start_unix_ns: tickertape::clock::realtime_ns(),
        };
        let mut state = RunState {
            reports: config
                .services
                .iter()
                .map(|s| ServiceReport {
                    name: s.name().to_string(),
                    descriptor_seq: None,
                    descriptors: 0,
                    messages: 0,
                    payload_bytes: 0,
                    buffer_size: None,
                    subscriber_max_buffer_size: None,
                    safe_overflow: None,
                    gaps: 0,
                    missing: 0,
                    backwards: 0,
                    errors: 0,
                    last_error: None,
                })
                .collect(),
            messages: 0,
            checkpoints: 0,
        };
        let signalled = signals.as_ref().map(|s| Arc::clone(&s.signalled));
        if let Some(f) = config.on_start.take() {
            f(run.start_seq);
        }
        let result = record(
            &mut config,
            &node,
            &mut journal,
            &run,
            &mut state,
            signalled.as_deref(),
        );
        // Make everything durable on the way out, also after an error: a blocking checkpoint, which first waits for a
        // background one in flight (and returns its failure if no request has).
        let checkpoint = journal.writer.checkpoint();
        drop(signals);
        let stop_reason = result?;
        checkpoint?;
        state.checkpoints += 1;
        for (i, report) in state.reports.iter_mut().enumerate() {
            report.descriptors = journal.descriptors(i);
        }
        Ok(RecorderReport {
            run_start_seq: run.start_seq,
            next_seq: journal.writer.next_seq(),
            messages: state.messages,
            checkpoints: state.checkpoints,
            fillers: journal.fillers(),
            stop_reason,
            services: state.reports,
        })
    }
}

struct RunInfo {
    start_seq: u64,
    start_unix_ns: u64,
}

struct RunState {
    reports: Vec<ServiceReport>,
    messages: u64,
    checkpoints: u64,
}

/// Messages taken from one service in one pass, before the checkpoint timer and stop conditions are looked at.
const DRAIN_BATCH: usize = 4096;

// `i` indexes the parallel vectors `config.services`, `sources`, `tracking` and `state.reports`.
#[allow(clippy::needless_range_loop)]
fn record(
    config: &mut RecorderBuilder,
    node: &Node<ipc::Service>,
    journal: &mut Journal,
    run: &RunInfo,
    state: &mut RunState,
    signalled: Option<&AtomicBool>,
) -> Result<StopReason, Error> {
    let started = Instant::now();
    let mut last_checkpoint = Instant::now();
    let n = config.services.len();
    let mut sources: Vec<Option<Box<dyn Source>>> = (0..n).map(|_| None).collect();
    let mut tracking: Vec<Option<Tracking>> = (0..n).map(|_| None).collect();
    let mut failed = vec![false; n];
    loop {
        // Close services that failed, then open what can be opened, writing a descriptor for each.
        for i in 0..n {
            if std::mem::take(&mut failed[i]) {
                sources[i] = None;
                tracking[i] = None;
                journal.close_service(i);
            }
        }
        for i in 0..n {
            if sources[i].is_some() {
                continue;
            }
            let opened = config.services[i].open(node, config.buffer_size);
            let opened = opened.and_then(|o| match o {
                Opened::NotYet => Ok(None),
                Opened::Ready(source) => activate(
                    config,
                    i,
                    source.as_ref(),
                    journal,
                    run,
                    &mut tracking,
                    state,
                )
                .map(|()| Some(source)),
            });
            match opened {
                Ok(Some(source)) => sources[i] = Some(source),
                Ok(None) => {}
                Err(e) if e.is_journal_error() => return Err(e),
                Err(e) => service_failed(config, i, &e, state),
            }
        }
        let pending = sources.iter().any(Option::is_none);
        let retry_at = deadline_after(config.retry_interval);

        // Wait on a WaitSet built for the services open now; rebuild it when one opens or fails.
        let waitset = WaitSetBuilder::new()
            .signal_handling_mode(SignalHandlingMode::Disabled)
            .create::<ipc::Service>()
            .map_err(|e| Error::iox("create waitset", e))?;
        let _tick = waitset
            .attach_interval(config.poll_interval)
            .map_err(|e| Error::iox("attach interval", e))?;
        let mut guards = Vec::new();
        for source in sources.iter().flatten() {
            if let Some(listener) = source.listener() {
                guards.push(
                    waitset
                        .attach_notification(listener)
                        .map_err(|e| Error::iox("attach listener", e))?,
                );
            }
        }
        let mut busy = false;
        loop {
            if !busy {
                let waited = waitset.wait_and_process_once(|_| CallbackProgression::Continue);
                if let Err(e) = waited
                    && !config.stop.load(Ordering::Relaxed)
                {
                    return Err(Error::iox("wait", e));
                }
            }
            for source in sources.iter().flatten() {
                if let Some(listener) = source.listener() {
                    // Consume notifications so the WaitSet doesn't wake again for them.
                    let _ = listener.try_wait(|_| {});
                }
            }
            let budget = DRAIN_BATCH;
            busy = drain_all(
                config,
                &sources,
                &mut tracking,
                &mut failed,
                journal,
                state,
                budget,
            )?;
            if last_checkpoint.elapsed() >= config.checkpoint_interval {
                // In the background: recording goes on while it syncs. A failure of the previous one is returned
                // here, once, and stops the recorder (the final checkpoint then reports `DurabilityLost`).
                journal.writer.request_checkpoint()?;
                state.checkpoints += 1;
                last_checkpoint = Instant::now();
            }
            if let Some(reason) = stop_reason(config, state, started, signalled) {
                // Take what is already buffered before stopping (at most one buffer per service), unless a message
                // limit was reached.
                if reason != StopReason::MaxMessages {
                    drain_all(
                        config,
                        &sources,
                        &mut tracking,
                        &mut failed,
                        journal,
                        state,
                        config.buffer_size,
                    )?;
                }
                return Ok(reason);
            }
            if failed.iter().any(|f| *f)
                || (pending && retry_at.is_some_and(|r| Instant::now() >= r))
            {
                break;
            }
        }
    }
}

/// Writes the first descriptor of a newly opened service and starts tracking it.
fn activate(
    config: &mut RecorderBuilder,
    i: usize,
    source: &dyn Source,
    journal: &mut Journal,
    run: &RunInfo,
    tracking: &mut [Option<Tracking>],
    state: &mut RunState,
) -> Result<(), Error> {
    let factory = &config.services[i];
    let d = source.described();
    let descriptor = Descriptor {
        run_start_seq: run.start_seq,
        run_start_unix_ns: run.start_unix_ns,
        imported: false,
        iceoryx2_version: d.iceoryx2_version,
        settings: d.settings,
        recorder_buffer_size: d.buffer_size as u64,
        counter: factory.counter(),
        payload: d.payload.clone(),
        user_header: d.user_header.clone(),
        system_header: d.system_header.clone(),
        service_name: factory.name().to_string(),
        recorder: crate::recorder_name(),
    };
    // Validated here, before anything is appended: a service whose types or settings the layout doesn't accept is
    // refused (and retried), the others keep recording.
    let bytes = descriptor.encode().map_err(|e| Error::Unsupported {
        service: factory.name().to_string(),
        reason: e.to_string(),
    })?;
    let seq = journal.open_service(i, bytes, tickertape::clock::realtime_ns())?;
    let report = &mut state.reports[i];
    report.descriptor_seq.get_or_insert(seq);
    report.descriptors = journal.descriptors(i);
    report.buffer_size = Some(d.buffer_size);
    report.subscriber_max_buffer_size = Some(d.settings.subscriber_max_buffer_size);
    report.safe_overflow = Some(d.settings.safe_overflow);
    tracking[i] = Some(Tracking {
        descriptor: Arc::new(descriptor),
        counter: factory.counter(),
        next_counter: HashMap::new(),
    });
    let name = factory.name().to_string();
    if let Some(f) = config.on_open.as_mut() {
        f(&name, seq);
    }
    Ok(())
}

fn service_failed(config: &mut RecorderBuilder, i: usize, e: &Error, state: &mut RunState) {
    let report = &mut state.reports[i];
    report.errors += 1;
    let text = e.to_string();
    let new = report.last_error.as_deref() != Some(text.as_str());
    report.last_error = Some(text);
    if new && let Some(f) = config.on_error.as_mut() {
        f(&report.name, e);
    }
}

fn stop_reason(
    config: &RecorderBuilder,
    state: &RunState,
    started: Instant,
    signalled: Option<&AtomicBool>,
) -> Option<StopReason> {
    if config.stop.load(Ordering::Relaxed) {
        return Some(if signalled.is_some_and(|s| s.load(Ordering::Relaxed)) {
            StopReason::Signal
        } else {
            StopReason::StopFlag
        });
    }
    if config.max_messages.is_some_and(|m| state.messages >= m) {
        return Some(StopReason::MaxMessages);
    }
    if config.max_duration.is_some_and(|d| started.elapsed() >= d) {
        return Some(StopReason::MaxDuration);
    }
    None
}

/// One pass over all open services, at most `budget` messages each. Returns true if some service may have more
/// (it filled its budget). A service whose receive fails is marked in `failed`.
fn drain_all(
    config: &mut RecorderBuilder,
    sources: &[Option<Box<dyn Source>>],
    tracking: &mut [Option<Tracking>],
    failed: &mut [bool],
    journal: &mut Journal,
    state: &mut RunState,
    budget: usize,
) -> Result<bool, Error> {
    let mut busy = false;
    for (i, source) in sources.iter().enumerate() {
        let Some(source) = source else { continue };
        if failed[i] || tracking[i].is_none() {
            continue;
        }
        let budget = match config.max_messages {
            Some(m) => usize::try_from(m.saturating_sub(state.messages))
                .unwrap_or(usize::MAX)
                .min(budget),
            None => budget,
        };
        if budget == 0 {
            return Ok(busy);
        }
        let mut journal_error = None;
        let drained = source.drain(budget, &mut |m| {
            record_message(i, &m, journal, tracking, state).inspect_err(|e| {
                if e.is_journal_error() {
                    journal_error = Some(());
                }
            })
        });
        match drained {
            Ok(n) => busy |= n >= budget,
            Err(e) if journal_error.is_some() => return Err(e),
            Err(e) => {
                failed[i] = true;
                service_failed(config, i, &e, state);
            }
        }
    }
    Ok(busy)
}

/// Appends one message (and a loss record before it if its counter shows a gap). Everything is checked before
/// anything is appended.
fn record_message(
    i: usize,
    m: &Message<'_>,
    journal: &mut Journal,
    tracking: &mut [Option<Tracking>],
    state: &mut RunState,
) -> Result<(), Error> {
    let local_ts_ns = tickertape::clock::realtime_ns();
    let Some(track) = tracking[i].as_ref() else {
        return Ok(());
    };
    // The message must fit the service's descriptor (it always does for a source opened with those types).
    layout::check_message(m.user_header, m.payload, &track.descriptor).map_err(|e| {
        Error::Unsupported {
            service: track.descriptor.service_name.clone(),
            reason: e.to_string(),
        }
    })?;
    let data_len = DataRecord {
        descriptor_seq: 0,
        publisher_id: m.origin,
        user_header: m.user_header,
        payload: m.payload,
    }
    .encoded_len()
    .map_err(|e| Error::Unsupported {
        service: track.descriptor.service_name.clone(),
        reason: e.to_string(),
    })?;
    let loss = track.counter.and_then(|c| {
        let value = c.read(m.user_header, m.payload)?;
        let expected = *track.next_counter.get(&m.origin)?;
        LossKind::classify(c.width, expected, value).map(|kind| LossRecord {
            kind,
            width: c.width,
            descriptor_seq: 0,
            publisher_id: m.origin,
            expected,
            received: value,
        })
    });
    if let Some(loss) = loss {
        // Checked before anything is appended (the descriptor's sequence number doesn't affect validity).
        loss.encode().map_err(|e| Error::Unsupported {
            service: track.descriptor.service_name.clone(),
            reason: e.to_string(),
        })?;
        journal.append_named(
            i,
            &NewRecord {
                schema_id: SCHEMA_LOSS,
                venue_ts_ns: 0,
                local_ts_ns,
                payload: &[],
            },
            layout::LOSS_RECORD_LEN,
            |buf, descriptor_seq| {
                // Checked above, so this can't fail; if it did, the panic makes the writer discard the record
                // instead of committing it.
                let bytes = LossRecord {
                    descriptor_seq,
                    ..loss
                }
                .encode()
                .expect("loss record checked");
                buf.copy_from_slice(&bytes);
            },
        )?;
        let report = &mut state.reports[i];
        match loss.kind {
            LossKind::Gap => {
                report.gaps += 1;
                report.missing = report.missing.saturating_add(loss.missing());
            }
            LossKind::Backwards => report.backwards += 1,
        }
    }
    journal.append_named(
        i,
        &NewRecord {
            schema_id: SCHEMA_DATA,
            venue_ts_ns: m.venue_ts_ns,
            local_ts_ns,
            payload: &[],
        },
        data_len,
        |buf, descriptor_seq| {
            // The length was checked above, so this can't fail; if it did, the panic makes the writer discard the
            // record instead of committing it.
            DataRecord {
                descriptor_seq,
                publisher_id: m.origin,
                user_header: m.user_header,
                payload: m.payload,
            }
            .encode_into(buf)
            .expect("data record length checked")
        },
    )?;
    let Some(track) = tracking[i].as_mut() else {
        return Ok(());
    };
    if let Some(c) = track.counter
        && let Some(value) = c.read(m.user_header, m.payload)
    {
        track
            .next_counter
            .insert(m.origin, value.wrapping_add(1) & c.max_value());
    }
    let report = &mut state.reports[i];
    report.messages += 1;
    report.payload_bytes = report.payload_bytes.saturating_add(m.payload.len() as u64);
    state.messages += 1;
    Ok(())
}

/// `SIGINT`/`SIGTERM` handling for one run (see [`RecorderBuilder::handle_signals`] for what the process sees): the
/// first signal the run's signal thread takes sets the stop flag and this run's own `signalled` flag; a signal it
/// takes after that exits at once (status 130 or 143). Signals are taken one at a time by that thread (signal-hook's
/// iterator), so two signals can't both look like the first; but signals of one kind that arrive before the thread
/// has taken the previous one are merged into one, by the iterator and by the operating system.
///
/// While no run handles signals, `SIGINT` and `SIGTERM` take their default action (they end the process): the first
/// registration in a process installs, for the rest of its life, a conditional default action for both (signal-hook's
/// `register_conditional_default`), armed whenever no registration is alive.
pub(crate) struct Signals {
    handle: signal_hook::iterator::Handle,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Set on the first signal, so a stop by signal can be told from a stop flag. Belongs to this registration only.
    pub(crate) signalled: Arc<AtomicBool>,
}

/// The process-wide state behind [`Signals`]: the flag that arms the default action, and how many registrations are
/// alive.
struct SignalDefaults {
    idle: Arc<AtomicBool>,
    active: usize,
}

static SIGNAL_DEFAULTS: std::sync::Mutex<Option<SignalDefaults>> = std::sync::Mutex::new(None);

fn signal_error(source: std::io::Error) -> Error {
    Error::Io {
        op: "install signal handler",
        path: PathBuf::new(),
        source,
    }
}

impl Signals {
    pub(crate) fn register(stop: &Arc<AtomicBool>) -> Result<Signals, Error> {
        use signal_hook::consts::{SIGINT, SIGTERM};
        {
            let mut defaults = SIGNAL_DEFAULTS.lock().unwrap_or_else(|e| e.into_inner());
            let defaults = match &mut *defaults {
                Some(d) => d,
                None => {
                    let idle = Arc::new(AtomicBool::new(false));
                    for signal in [SIGINT, SIGTERM] {
                        signal_hook::flag::register_conditional_default(signal, Arc::clone(&idle))
                            .map_err(signal_error)?;
                    }
                    defaults.insert(SignalDefaults { idle, active: 0 })
                }
            };
            defaults.active += 1;
            defaults.idle.store(false, Ordering::SeqCst);
        }
        let registered = Self::register_iterator(stop);
        if registered.is_err() {
            Self::release();
        }
        registered
    }

    fn register_iterator(stop: &Arc<AtomicBool>) -> Result<Signals, Error> {
        use signal_hook::consts::{SIGINT, SIGTERM};
        let mut signals =
            signal_hook::iterator::Signals::new([SIGINT, SIGTERM]).map_err(signal_error)?;
        let handle = signals.handle();
        let signalled = Arc::new(AtomicBool::new(false));
        let (seen, stop) = (Arc::clone(&signalled), Arc::clone(stop));
        let thread = std::thread::Builder::new()
            .name("tickertape-iox2-signals".into())
            .spawn(move || {
                for signal in signals.forever() {
                    if seen.swap(true, Ordering::SeqCst) {
                        signal_hook::low_level::exit(if signal == SIGINT { 130 } else { 143 });
                    }
                    stop.store(true, Ordering::SeqCst);
                }
                // `signals` is dropped here, which unregisters the handlers.
            })
            .map_err(signal_error)?;
        Ok(Signals {
            handle,
            thread: Some(thread),
            signalled,
        })
    }

    /// One registration fewer; with none left, arms the default action.
    fn release() {
        let mut defaults = SIGNAL_DEFAULTS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(d) = &mut *defaults {
            d.active = d.active.saturating_sub(1);
            if d.active == 0 {
                d.idle.store(true, Ordering::SeqCst);
            }
        }
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        // Arm the default action first: a signal arriving from here on ends the process (the run is over).
        Self::release();
        self.handle.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::TypeVariant;

    fn tracking_for(descriptor: Descriptor) -> Tracking {
        Tracking {
            descriptor: Arc::new(descriptor),
            counter: None,
            next_counter: HashMap::new(),
        }
    }

    fn descriptor(user_header_size: u64) -> Descriptor {
        let t = |name: &str, size| TypeInfo {
            variant: TypeVariant::FixedSize,
            name: name.into(),
            size,
            alignment: 1,
        };
        Descriptor {
            run_start_seq: 0,
            run_start_unix_ns: 0,
            imported: false,
            iceoryx2_version: Version {
                major: 0,
                minor: 10,
                patch: 0,
            },
            settings: ServiceSettings::UNKNOWN,
            recorder_buffer_size: 0,
            counter: None,
            payload: t("P", 8),
            user_header: t("H", user_header_size),
            system_header: t("S", 48),
            service_name: "s".into(),
            recorder: "test".into(),
        }
    }

    fn report() -> ServiceReport {
        ServiceReport {
            name: "s".into(),
            descriptor_seq: None,
            descriptors: 0,
            messages: 0,
            payload_bytes: 0,
            buffer_size: None,
            subscriber_max_buffer_size: None,
            safe_overflow: None,
            gaps: 0,
            missing: 0,
            backwards: 0,
            errors: 0,
            last_error: None,
        }
    }

    /// A message the encoder or the descriptor doesn't accept is refused before anything is appended: no
    /// zero-filled or mismatched record is committed.
    #[test]
    fn invalid_messages_are_refused_before_appending() {
        let dir = tempfile::tempdir().unwrap();
        let config = WriterConfig::default().segment_size(65_536).prefault(false);
        let mut journal = Journal::open(dir.path(), config, 1).unwrap();
        let d = descriptor(16);
        journal.open_service(0, d.encode().unwrap(), 1).unwrap();
        let mut tracking = vec![Some(tracking_for(d))];
        let mut state = RunState {
            reports: vec![report()],
            messages: 0,
            checkpoints: 0,
        };
        // A user header longer than the descriptor's (as a header too long for the encoder would be).
        let err = record_message(
            0,
            &Message {
                user_header: &[0; 17],
                payload: &[0; 8],
                origin: 1,
                venue_ts_ns: 0,
            },
            &mut journal,
            &mut tracking,
            &mut state,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("user header has 17 bytes"),
            "{err}"
        );
        assert_eq!(journal.writer.next_seq(), 1, "nothing was appended");
        record_message(
            0,
            &Message {
                user_header: &[0; 16],
                payload: &[0; 8],
                origin: 1,
                venue_ts_ns: 0,
            },
            &mut journal,
            &mut tracking,
            &mut state,
        )
        .unwrap();
        assert_eq!(journal.writer.next_seq(), 2);
        assert_eq!(state.messages, 1);
    }

    /// Counters are compared modulo their width: a four-byte counter that wraps is not a loss; one that goes back
    /// is; a gap across the wrap counts the modular distance.
    #[test]
    fn deadlines_saturate_to_none() {
        assert!(deadline_after(Duration::MAX).is_none());
        assert!(deadline_after(Duration::from_secs(u64::MAX)).is_none());
        let soon = deadline_after(Duration::from_millis(10)).unwrap();
        assert!(soon > Instant::now() - Duration::from_secs(1));
    }

    #[test]
    fn counters_use_modular_distance() {
        let dir = tempfile::tempdir().unwrap();
        let config = WriterConfig::default().segment_size(65_536).prefault(false);
        let mut journal = Journal::open(dir.path(), config, 1).unwrap();
        let mut d = descriptor(16);
        d.counter = Some(CounterField {
            location: layout::CounterLocation::UserHeader,
            offset: 0,
            width: 4,
        });
        journal.open_service(0, d.encode().unwrap(), 1).unwrap();
        let mut t = tracking_for(d.clone());
        t.counter = d.counter;
        let mut tracking = vec![Some(t)];
        let mut state = RunState {
            reports: vec![report()],
            messages: 0,
            checkpoints: 0,
        };
        let mut send = |counter: u32| {
            let mut header = [0u8; 16];
            header[..4].copy_from_slice(&counter.to_le_bytes());
            record_message(
                0,
                &Message {
                    user_header: &header,
                    payload: &[0; 8],
                    origin: 9,
                    venue_ts_ns: 0,
                },
                &mut journal,
                &mut tracking,
                &mut state,
            )
            .unwrap();
        };
        send(u32::MAX - 1);
        send(u32::MAX);
        send(0); // wraps: no loss
        send(3); // gap of 2 (1 and 2 missing)
        send(2); // backwards
        send(u32::MAX - 1); // backwards (a restart, far behind)
        send(1); // gap across the wrap: MAX, 0 missing
        let r = &state.reports[0];
        assert_eq!((r.gaps, r.missing, r.backwards), (2, 4, 2), "{r:?}");
    }

    /// A source that always has more: a service that keeps supplying full batches.
    struct Endless {
        described: Described,
    }

    impl Source for Endless {
        fn described(&self) -> &Described {
            &self.described
        }

        fn listener(&self) -> Option<&Listener<ipc::Service>> {
            None
        }

        fn drain(
            &self,
            max: usize,
            f: &mut dyn FnMut(Message<'_>) -> Result<(), Error>,
        ) -> Result<usize, Error> {
            for _ in 0..max {
                f(Message {
                    user_header: &[0; 16],
                    payload: &[0; 8],
                    origin: 1,
                    venue_ts_ns: 0,
                })?;
            }
            Ok(max)
        }
    }

    /// One pass takes at most one batch from each service, even from services that never run dry, so the checkpoint
    /// timer, the stop flag and the other services are looked at after every pass.
    #[test]
    fn one_pass_takes_a_bounded_batch_from_each_service() {
        let dir = tempfile::tempdir().unwrap();
        let config = WriterConfig::default()
            .segment_size(1 << 24)
            .prefault(false);
        let mut journal = Journal::open(dir.path(), config, 2).unwrap();
        let d = descriptor(16);
        let described = || Described {
            payload: d.payload.clone(),
            user_header: d.user_header.clone(),
            system_header: d.system_header.clone(),
            settings: d.settings,
            iceoryx2_version: d.iceoryx2_version,
            buffer_size: 16,
        };
        let sources: Vec<Option<Box<dyn Source>>> = vec![
            Some(Box::new(Endless {
                described: described(),
            })),
            Some(Box::new(Endless {
                described: described(),
            })),
        ];
        let mut tracking = vec![Some(tracking_for(d.clone())), Some(tracking_for(d.clone()))];
        journal.open_service(0, d.encode().unwrap(), 1).unwrap();
        journal.open_service(1, d.encode().unwrap(), 1).unwrap();
        let mut state = RunState {
            reports: vec![report(), report()],
            messages: 0,
            checkpoints: 0,
        };
        let mut failed = vec![false; 2];
        let mut config = Recorder::builder(dir.path());
        let busy = drain_all(
            &mut config,
            &sources,
            &mut tracking,
            &mut failed,
            &mut journal,
            &mut state,
            DRAIN_BATCH,
        )
        .unwrap();
        assert!(busy, "both services have more");
        assert_eq!(state.reports[0].messages, DRAIN_BATCH as u64);
        assert_eq!(state.reports[1].messages, DRAIN_BATCH as u64);
    }
}
