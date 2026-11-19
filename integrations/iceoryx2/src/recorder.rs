//! Record iceoryx2 publish/subscribe services into a tickertape journal.
//!
//! [`Recorder`] subscribes to each configured service and appends one tickertape record per message (see
//! [`layout`](crate::layout) for the bytes). It is the journal's only writer while it runs.
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
//! between wake-ups; it never spins.
//!
//! # Buffer size, overflow and loss
//!
//! Between wake-ups, messages wait in the recorder's subscriber buffer, whose size is the smaller of
//! [`RecorderBuilder::buffer_size`] (1,024 by default) and the service's `subscriber_max_buffer_size`. iceoryx2's
//! default for the latter is 2. If the recorder creates a service, it creates it with `subscriber_max_buffer_size`
//! equal to its buffer size; if a publisher created it first, the publisher's setting wins. When the buffer is
//! full, a service with safe overflow (iceoryx2's default) replaces the oldest unread sample, which the recorder
//! never sees; without safe overflow, the publisher's backpressure strategy decides (block, retry or discard).
//! The descriptor records both the service's settings and the buffer size the recorder got. To detect loss,
//! give each publisher a counter and configure a [`CounterField`]: each gap becomes a
//! [`crate::layout::LossRecord`].
//!
//! # Sessions, restarts and durability
//!
//! Each run is a session: it writes one descriptor per service before that service's first message. A recorder
//! restarted on the same journal continues it after recovery; tickertape sequence numbers stay gap-free. If the
//! service keeps history (`history_size > 0`), a reconnecting recorder receives that history again; with a counter
//! configured, such repeats show up as `backwards` loss records. Records are visible to readers as soon as they
//! are written and durable after a checkpoint: every [`RecorderBuilder::checkpoint_interval`] (1 s by default) and
//! once more on a clean stop (`SIGINT`, `SIGTERM`, a stop flag, or a limit).

use std::collections::HashMap;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use iceoryx2::port::listener::Listener;
use iceoryx2::port::subscriber::Subscriber;
use iceoryx2::prelude::*;
use iceoryx2::service::static_config::message_type_details::{
    TypeDetail, TypeVariant as IoxVariant,
};
use iceoryx2::service::static_config::publish_subscribe::StaticConfig as PubSubConfig;
use serde::Serialize;
use tickertape::{NewRecord, Writer, WriterConfig};

use crate::Error;
use crate::layout::{
    CounterField, DataRecord, Descriptor, LossKind, LossRecord, SCHEMA_DATA, SCHEMA_DESCRIPTOR,
    SCHEMA_LOSS, ServiceSettings, TypeInfo, TypeVariant, Version,
};
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

/// A configured service that may not exist yet.
pub(crate) trait SourceFactory: Send {
    fn name(&self) -> &str;
    fn counter(&self) -> Option<CounterField>;
    /// Opens the service, or returns `None` if it can't be opened yet (a runtime-typed service that doesn't exist).
    fn open(
        &mut self,
        node: &Node<ipc::Service>,
        buffer_size: usize,
    ) -> Result<Option<Box<dyn Source>>, Error>;
}

/// Reads `venue_ts_ns` from a typed user header.
type VenueTs<H> = Box<dyn Fn(&H) -> u64 + Send>;
/// Called when a service is opened.
type OnOpen = Box<dyn FnMut(&str, u64) + Send>;

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
    pub fn venue_ts(mut self, f: impl Fn(&H) -> u64 + Send + 'static) -> Self {
        self.venue_ts = Some(Box::new(f));
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

    fn open(
        &mut self,
        node: &Node<ipc::Service>,
        buffer_size: usize,
    ) -> Result<Option<Box<dyn Source>>, Error> {
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
        let subscriber = service
            .subscriber_builder()
            .buffer_size(buffer)
            .create()
            .map_err(|e| Error::iox("create subscriber", e))?;
        let described = describe(cfg, service_version(node, &name), buffer);
        let listener = open_listener(node, self.notify.as_deref())?;
        Ok(Some(Box::new(TypedSource {
            subscriber,
            venue_ts: self.venue_ts.take(),
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

pub(crate) fn service_name(name: &str) -> Result<ServiceName, Error> {
    ServiceName::new(name)
        .map_err(|e| Error::Usage(format!("invalid iceoryx2 service name \"{name}\": {e:?}")))
}

pub(crate) fn incompatible_or(name: &str, e: impl core::fmt::Debug) -> Error {
    let text = format!("{e:?}");
    if text.contains("Incompatible") {
        Error::Incompatible {
            service: name.to_string(),
            reason: format!("iceoryx2 refused to open it ({text})"),
        }
    } else {
        Error::iox(&format!("open service \"{name}\""), e)
    }
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
    /// `SIGINT` or `SIGTERM`.
    Signal,
    /// The stop flag was set.
    StopFlag,
    /// [`RecorderBuilder::max_messages`] were recorded.
    MaxMessages,
    /// [`RecorderBuilder::max_duration`] passed.
    MaxDuration,
}

/// Per-service results of a recording session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServiceReport {
    /// Service name.
    pub name: String,
    /// Sequence number of this session's descriptor, if the service was opened.
    pub descriptor_seq: Option<u64>,
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
    /// Messages missing according to those gaps.
    pub missing: u64,
    /// Counter regressions found (repeats, restarts).
    pub backwards: u64,
}

/// The results of [`Recorder::run`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecorderReport {
    /// This session's id: the journal's next sequence number when the recorder opened it.
    pub session_id: u64,
    /// The journal's next sequence number when the recorder stopped.
    pub next_seq: u64,
    /// Messages recorded, all services.
    pub messages: u64,
    /// Checkpoints taken, including the final one.
    pub checkpoints: u64,
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

    /// How often to checkpoint, making recorded messages durable. Default: 1 s.
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

    /// The subscriber buffer the recorder asks for, capped by the service's `subscriber_max_buffer_size`.
    /// Default: 1,024.
    pub fn buffer_size(mut self, size: usize) -> Self {
        self.buffer_size = size;
        self
    }

    /// The iceoryx2 node name. Default: `tickertape-recorder`.
    pub fn node_name(mut self, name: impl Into<String>) -> Self {
        self.node_name = name.into();
        self
    }

    /// Whether `SIGINT` and `SIGTERM` stop the recorder cleanly (a second one exits at once). Default: true.
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

    /// Called with the service name and descriptor sequence number when a service is opened and its descriptor
    /// written.
    pub fn on_open(mut self, f: impl FnMut(&str, u64) + Send + 'static) -> Self {
        self.on_open = Some(Box::new(f));
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
    /// [`Error::Usage`] if there are no services, a name is repeated, or an interval or the buffer size is zero.
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
            || self.buffer_size == 0
        {
            return Err(Error::Usage(
                "the checkpoint interval, poll interval and buffer size must be positive".into(),
            ));
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

/// Per-service bookkeeping, kept apart from the sources so the WaitSet can borrow those while this changes.
struct Tracking {
    descriptor_seq: u64,
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
        }
    }

    /// A flag that stops the recorder cleanly when set, from any thread.
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.config.stop)
    }

    /// Opens the journal and records until a signal, the stop flag or a limit. Services that can't be opened yet
    /// (a runtime-typed service whose publisher hasn't started) are retried every 100 ms.
    ///
    /// # Errors
    ///
    /// [`Error::Journal`] if the journal can't be opened or written, [`Error::Iceoryx2`] or
    /// [`Error::Incompatible`] if a service can't be opened. A final checkpoint is attempted before returning
    /// an append error.
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
        let mut writer = Writer::open(&config.journal, config.writer_config.clone())?;
        let session = Session {
            id: writer.next_seq(),
            start_unix_ns: tickertape::clock::realtime_ns(),
        };
        let mut state = RunState {
            reports: config
                .services
                .iter()
                .map(|s| ServiceReport {
                    name: s.name().to_string(),
                    descriptor_seq: None,
                    messages: 0,
                    payload_bytes: 0,
                    buffer_size: None,
                    subscriber_max_buffer_size: None,
                    safe_overflow: None,
                    gaps: 0,
                    missing: 0,
                    backwards: 0,
                })
                .collect(),
            messages: 0,
            checkpoints: 0,
        };
        let result = record(&mut config, &node, &mut writer, &session, &mut state);
        // Make everything durable on the way out, also after an error.
        let checkpoint = writer.checkpoint();
        drop(signals);
        let stop_reason = result?;
        checkpoint?;
        state.checkpoints += 1;
        Ok(RecorderReport {
            session_id: session.id,
            next_seq: writer.next_seq(),
            messages: state.messages,
            checkpoints: state.checkpoints,
            stop_reason,
            services: state.reports,
        })
    }
}

struct Session {
    id: u64,
    start_unix_ns: u64,
}

struct RunState {
    reports: Vec<ServiceReport>,
    messages: u64,
    checkpoints: u64,
}

const DRAIN_BATCH: usize = 4096;

fn record(
    config: &mut RecorderBuilder,
    node: &Node<ipc::Service>,
    writer: &mut Writer,
    session: &Session,
    state: &mut RunState,
) -> Result<StopReason, Error> {
    let started = Instant::now();
    let mut last_checkpoint = Instant::now();
    let mut sources: Vec<Option<Box<dyn Source>>> =
        (0..config.services.len()).map(|_| None).collect();
    let mut tracking: Vec<Option<Tracking>> = (0..config.services.len()).map(|_| None).collect();
    loop {
        // Open what can be opened, writing a descriptor for each.
        for (i, factory) in config.services.iter_mut().enumerate() {
            if sources[i].is_some() {
                continue;
            }
            if let Some(source) = factory.open(node, config.buffer_size)? {
                let d = source.described();
                let descriptor = Descriptor {
                    session_id: session.id,
                    session_start_unix_ns: session.start_unix_ns,
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
                let bytes = descriptor.encode().map_err(|source| Error::Layout {
                    seq: writer.next_seq(),
                    source,
                })?;
                let seq = writer.append(&NewRecord {
                    schema_id: SCHEMA_DESCRIPTOR,
                    venue_ts_ns: 0,
                    local_ts_ns: tickertape::clock::realtime_ns(),
                    payload: &bytes,
                })?;
                let report = &mut state.reports[i];
                report.descriptor_seq = Some(seq);
                report.buffer_size = Some(d.buffer_size);
                report.subscriber_max_buffer_size = Some(d.settings.subscriber_max_buffer_size);
                report.safe_overflow = Some(d.settings.safe_overflow);
                if let Some(f) = config.on_open.as_mut() {
                    f(factory.name(), seq);
                }
                sources[i] = Some(source);
                tracking[i] = Some(Tracking {
                    descriptor_seq: seq,
                    counter: factory.counter(),
                    next_counter: HashMap::new(),
                });
            }
        }
        let pending = sources.iter().any(Option::is_none);
        let rebuild_at = Instant::now() + config.retry_interval;

        // Wait on a WaitSet built for the services open now; rebuild it when more open.
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
        loop {
            let waited = waitset.wait_and_process_once(|_| CallbackProgression::Continue);
            if let Err(e) = waited
                && !config.stop.load(Ordering::Relaxed)
            {
                return Err(Error::iox("wait", e));
            }
            for source in sources.iter().flatten() {
                if let Some(listener) = source.listener() {
                    // Consume notifications so the WaitSet doesn't wake again for them.
                    let _ = listener.try_wait(|_| {});
                }
            }
            drain_all(&sources, &mut tracking, writer, state, config.max_messages)?;
            if last_checkpoint.elapsed() >= config.checkpoint_interval {
                writer.checkpoint()?;
                state.checkpoints += 1;
                last_checkpoint = Instant::now();
            }
            if let Some(reason) = stop_reason(config, state, started) {
                // Take what is already buffered before stopping, unless a message limit was reached.
                if reason != StopReason::MaxMessages {
                    drain_all(&sources, &mut tracking, writer, state, config.max_messages)?;
                }
                return Ok(reason);
            }
            if pending && Instant::now() >= rebuild_at {
                break;
            }
        }
    }
}

fn stop_reason(config: &RecorderBuilder, state: &RunState, started: Instant) -> Option<StopReason> {
    if config.stop.load(Ordering::Relaxed) {
        return Some(if signalled().load(Ordering::Relaxed) {
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

fn drain_all(
    sources: &[Option<Box<dyn Source>>],
    tracking: &mut [Option<Tracking>],
    writer: &mut Writer,
    state: &mut RunState,
    max_messages: Option<u64>,
) -> Result<(), Error> {
    for (i, (source, track)) in sources.iter().zip(tracking.iter_mut()).enumerate() {
        let (Some(source), Some(track)) = (source, track) else {
            continue;
        };
        loop {
            let budget = match max_messages {
                Some(m) => usize::try_from(m.saturating_sub(state.messages))
                    .unwrap_or(usize::MAX)
                    .min(DRAIN_BATCH),
                None => DRAIN_BATCH,
            };
            if budget == 0 {
                return Ok(());
            }
            let report = &mut state.reports[i];
            let messages = &mut state.messages;
            let Tracking {
                descriptor_seq,
                counter,
                next_counter,
            } = track;
            let n = source.drain(budget, &mut |m| {
                let local_ts_ns = tickertape::clock::realtime_ns();
                if let Some(c) = counter
                    && let Some(value) = c.read(m.user_header, m.payload)
                {
                    let mask = if c.width == 4 {
                        u64::from(u32::MAX)
                    } else {
                        u64::MAX
                    };
                    if let Some(&expected) = next_counter.get(&m.origin)
                        && value != expected
                    {
                        let loss = LossRecord {
                            kind: if value > expected {
                                LossKind::Gap
                            } else {
                                LossKind::Backwards
                            },
                            descriptor_seq: *descriptor_seq,
                            publisher_id: m.origin,
                            expected,
                            received: value,
                        };
                        match loss.kind {
                            LossKind::Gap => {
                                report.gaps += 1;
                                report.missing += loss.missing();
                            }
                            LossKind::Backwards => report.backwards += 1,
                        }
                        writer.append(&NewRecord {
                            schema_id: SCHEMA_LOSS,
                            venue_ts_ns: 0,
                            local_ts_ns,
                            payload: &loss.encode(),
                        })?;
                    }
                    next_counter.insert(m.origin, value.wrapping_add(1) & mask);
                }
                let record = DataRecord {
                    descriptor_seq: *descriptor_seq,
                    user_header: m.user_header,
                    payload: m.payload,
                };
                let len = DataRecord::encoded_len(m.user_header.len(), m.payload.len())
                    .ok_or_else(|| Error::Usage("message too large".into()))?;
                let mut layout_error = None;
                writer.append_with(
                    &NewRecord {
                        schema_id: SCHEMA_DATA,
                        venue_ts_ns: m.venue_ts_ns,
                        local_ts_ns,
                        payload: &[],
                    },
                    len,
                    |buf| layout_error = record.encode_into(buf).err(),
                )?;
                if let Some(source) = layout_error {
                    return Err(Error::Layout {
                        seq: writer.next_seq() - 1,
                        source,
                    });
                }
                report.messages += 1;
                report.payload_bytes += m.payload.len() as u64;
                *messages += 1;
                Ok(())
            })?;
            if n < budget {
                break;
            }
        }
    }
    Ok(())
}

/// Set by the signal handlers, so a stop by signal can be told from a stop flag.
pub(crate) fn signalled() -> &'static Arc<AtomicBool> {
    static FLAG: std::sync::OnceLock<Arc<AtomicBool>> = std::sync::OnceLock::new();
    FLAG.get_or_init(|| Arc::new(AtomicBool::new(false)))
}

/// `SIGINT`/`SIGTERM` handlers that set the stop flag; a second signal exits at once (status 130 or 143).
pub(crate) struct Signals {
    ids: Vec<signal_hook::SigId>,
}

impl Signals {
    pub(crate) fn register(stop: &Arc<AtomicBool>) -> Result<Signals, Error> {
        use signal_hook::consts::{SIGINT, SIGTERM};
        let io = |source| Error::Io {
            op: "install signal handler",
            path: PathBuf::new(),
            source,
        };
        let mut ids = Vec::new();
        for (sig, code) in [(SIGINT, 130), (SIGTERM, 143)] {
            ids.push(
                signal_hook::flag::register_conditional_shutdown(sig, code, Arc::clone(stop))
                    .map_err(io)?,
            );
            ids.push(signal_hook::flag::register(sig, Arc::clone(stop)).map_err(io)?);
            ids.push(signal_hook::flag::register(sig, Arc::clone(signalled())).map_err(io)?);
        }
        Ok(Signals { ids })
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        for id in self.ids.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}
