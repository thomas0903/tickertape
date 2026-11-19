//! Typed payloads: byte views through `zerocopy`, so the typed recorder and replayer need no `unsafe`.

use core::fmt::Debug;

use iceoryx2::port::publisher::Publisher;
use iceoryx2::port::subscriber::Subscriber;
use iceoryx2::prelude::*;
use zerocopy::{FromBytes, Immutable, IntoBytes};

use crate::Error;
use crate::layout::{self, ChunkLayout, PortAllocation, ServiceSettings};
use crate::layout::{TypeInfo, Version};
use crate::recorder::{describe, type_info};
use iceoryx2::service::static_config::message_type_details::{
    TypeDetail, TypeVariant as IoxVariant,
};

/// The user header type details iceoryx2 derives for `H`.
pub(crate) fn header_type<H: RecordableHeader>() -> TypeInfo {
    type_info(&TypeDetail::new::<H>(IoxVariant::FixedSize))
}
use iceoryx2::service::static_config::publish_subscribe::StaticConfig as PubSubConfig;

/// Loaned samples the replayer's publishers are created with (iceoryx2's built-in default), so that the samples they
/// preallocate don't depend on a configuration file.
pub(crate) const PUBLISHER_MAX_LOANED: usize = 2;
/// True if publishers that don't choose an allocation strategy (fixed-size payloads) get a growing one from the
/// node's configuration.
pub(crate) fn default_strategy_grows(node: &Node<ipc::Service>) -> bool {
    node.config()
        .defaults
        .publish_subscribe
        .publisher_allocation_strategy
        != AllocationStrategy::Static
}

/// The dynamic configuration of a publish-subscribe service (its publishers and subscribers).
type PubSubDynamic = iceoryx2::service::dynamic_config::publish_subscribe::DynamicConfig<
    <ipc::Service as iceoryx2::service::Service>::Bag,
>;

/// Checks the settings in force of the service `display` (`cfg`) against this crate's caps
/// ([`ServiceSettings::validate`](layout::ServiceSettings::validate)), before anything that iceoryx2 sizes by them
/// (listing its ports or nodes, creating a port) runs.
pub(crate) fn check_settings(display: &str, cfg: &PubSubConfig) -> Result<(), Error> {
    let unknown = Version {
        major: 0,
        minor: 0,
        patch: 0,
    };
    describe(cfg, unknown, 0)
        .settings
        .validate()
        .map_err(|e| Error::Unsupported {
            service: display.to_string(),
            reason: format!("the service's settings in force: {e}"),
        })
}

/// Checks what the recorder's subscriber of the service `display`, with a buffer of `buffer` samples, will make
/// iceoryx2 allocate, with the service's settings in force (`cfg`), before it is created. First, from the settings
/// alone: the types, the settings' caps and its heap, which counts a connection slot with a growing publisher's
/// segment table for every publisher the service allows (`max_publishers`) and every expired connection, so it
/// bounds publishers that connect later too (see [`layout::subscriber_heap`]). Only then are the publishers present
/// listed (iceoryx2 allocates that list in proportion to `max_publishers`), and the connections the subscriber
/// creates to them checked, sized by what each of them advertises (see [`layout::subscriber_allocation`]).
pub(crate) fn check_subscriber(
    node: &Node<ipc::Service>,
    display: &str,
    cfg: &PubSubConfig,
    buffer: usize,
    dynamic: &PubSubDynamic,
) -> Result<(), Error> {
    let unknown = Version {
        major: 0,
        minor: 0,
        patch: 0,
    };
    let described = describe(cfg, unknown, 0);
    let unsupported = |reason: String| Error::Unsupported {
        service: display.to_string(),
        reason,
    };
    layout::validate_types(
        &described.payload,
        &described.user_header,
        &described.system_header,
    )
    .map_err(|e| unsupported(e.to_string()))?;
    let expired = node
        .config()
        .defaults
        .publish_subscribe
        .subscriber_expired_connection_buffer;
    let in_force = |e| unsupported(format!("the service's settings in force: {e}"));
    described.settings.validate().map_err(in_force)?;
    layout::subscriber_heap(&described.settings, expired as u64).map_err(|e| {
        // The settings are within their caps, so this is the heap, which max_publishers drives.
        let fit = layout::subscriber_max_publishers(&described.settings, expired as u64);
        unsupported(format!(
            "the service's settings in force: {e}; with an expired-connection buffer of {expired} and these \
             settings at most {fit} for max_publishers fits (203 with iceoryx2's default buffer of 128)"
        ))
    })?;
    let mut publishers = Vec::new();
    dynamic.list_publishers(|p| {
        publishers.push(layout::AdvertisedPublisher {
            samples: p.number_of_samples as u64,
            segments: u64::from(p.max_number_of_segments),
        });
        CallbackProgression::Continue
    });
    layout::subscriber_allocation(
        &described.settings,
        expired as u64,
        buffer as u64,
        &publishers,
    )
    .map(|_| ())
    .map_err(|e| {
        unsupported(format!(
            "the service's settings in force and its publishers: {e}"
        ))
    })
}

/// The slice length the replayer's slice publishers of the service `cfg` start with: room for about 4 KiB of
/// elements as iceoryx2 lays them out (the element size rounded up to the payload's alignment, which iceoryx2's
/// `payload_alignment` can raise above the size), at least one.
pub(crate) fn initial_slice_len(cfg: &PubSubConfig) -> usize {
    let payload = cfg.message_type_details().payload;
    let stride = payload
        .size()
        .max(1)
        .checked_next_multiple_of(payload.alignment().max(1))
        .unwrap_or(usize::MAX);
    (4096 / stride).max(1)
}

/// How the replayer's publisher of a service replayed by its recorded type details ([`Replayer::run_dynamic`]) is
/// created: (growing, initial elements). A fixed-size payload gets a static strategy, a slice a growing one that
/// starts with [`initial_slice_len`] elements.
///
/// [`Replayer::run_dynamic`]: crate::replayer::Replayer::run_dynamic
pub(crate) fn recorded_types_publisher(cfg: &PubSubConfig, slice: bool) -> (bool, usize) {
    if slice {
        (true, initial_slice_len(cfg))
    } else {
        (false, 1)
    }
}

/// The settings a service created by [`open_or_create!`] with `create` gets from the node `node`: the node's
/// configured defaults (`defaults.publish_subscribe`, what iceoryx2's builder starts from), overridden by what
/// `create` asks for, and raised to 1 where iceoryx2 0.10.0 raises a 0 to 1 when it creates the service
/// (`adjust_configuration_to_meaningful_values` in `service/builder/publish_subscribe.rs`).
pub(crate) fn settings_to_create(node: &Node<ipc::Service>, create: Create) -> ServiceSettings {
    let d = &node.config().defaults.publish_subscribe;
    let mut s = ServiceSettings {
        history_size: d.publisher_history_size as u64,
        subscriber_max_buffer_size: d.subscriber_max_buffer_size as u64,
        subscriber_max_borrowed_samples: d.subscriber_max_borrowed_samples as u64,
        max_publishers: d.max_publishers as u64,
        max_subscribers: d.max_subscribers as u64,
        max_nodes: d.max_nodes as u64,
        safe_overflow: d.enable_safe_overflow,
    };
    match create {
        Create::Recorder { buffer } => s.subscriber_max_buffer_size = buffer as u64,
        Create::Replay(r) => {
            // As `open_or_create!` passes them: history and overflow always, the others where recorded (not 0).
            s.history_size = r.history_size;
            s.safe_overflow = r.safe_overflow;
            let known = |recorded: u64, default: u64| if recorded > 0 { recorded } else { default };
            s.subscriber_max_buffer_size =
                known(r.subscriber_max_buffer_size, s.subscriber_max_buffer_size);
            s.subscriber_max_borrowed_samples = known(
                r.subscriber_max_borrowed_samples,
                s.subscriber_max_borrowed_samples,
            );
            s.max_publishers = known(r.max_publishers, s.max_publishers);
            s.max_subscribers = known(r.max_subscribers, s.max_subscribers);
            s.max_nodes = known(r.max_nodes, s.max_nodes);
        }
    }
    for v in [
        &mut s.subscriber_max_buffer_size,
        &mut s.subscriber_max_borrowed_samples,
        &mut s.max_publishers,
        &mut s.max_subscribers,
        &mut s.max_nodes,
    ] {
        *v = (*v).max(1);
    }
    s
}

/// Checks the settings a service created with `create` would get ([`settings_to_create`]) against this crate's caps,
/// before it is created: iceoryx2 sizes the new service's shared configuration by them (`max_publishers`,
/// `max_subscribers`, `max_nodes`), and a configured default may be far above the caps.
pub(crate) fn check_before_create(
    node: &Node<ipc::Service>,
    display: &str,
    create: Create,
) -> Result<(), Error> {
    settings_to_create(node, create)
        .validate()
        .map_err(|e| Error::Unsupported {
            service: display.to_string(),
            reason: format!(
                "the settings it would be created with (iceoryx2's configured defaults where none is given): {e}"
            ),
        })
}

/// What a port of an opened (or created) service will make iceoryx2 allocate, checked before the port is created;
/// see [memory a port may make iceoryx2 allocate](crate::layout#memory-a-port-may-make-iceoryx2-allocate).
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct PortBudget {
    allocation: PortAllocation,
    chunk: ChunkLayout,
    element_size: u64,
    /// The chunk size the publisher is created with (its first data segment's buckets).
    initial_chunk: u64,
}

impl PortBudget {
    /// Checks the settings and types in force of the service `display` (`cfg`, from the port factory: what iceoryx2
    /// opened or created, defaults included) for a publisher with `max_loaned` loaned samples, a growing allocation
    /// strategy (255 data segments) or a static one, that starts with `initial_elements` (its first data segment is
    /// checked too): its heap, its connections and its data segment (see [`layout::publisher_allocation`]).
    pub(crate) fn check(
        display: &str,
        cfg: &PubSubConfig,
        max_loaned: usize,
        growing: bool,
        initial_elements: usize,
    ) -> Result<PortBudget, Error> {
        let unsupported = |reason: String| Error::Unsupported {
            service: display.to_string(),
            reason,
        };
        let unknown = Version {
            major: 0,
            minor: 0,
            patch: 0,
        };
        let described = describe(cfg, unknown, 0);
        let chunk = layout::validate_types(
            &described.payload,
            &described.user_header,
            &described.system_header,
        )
        .map_err(|e| unsupported(e.to_string()))?;
        let allocation =
            layout::publisher_allocation(&described.settings, max_loaned as u64, growing)
                .map_err(|e| unsupported(format!("the service's settings in force: {e}")))?;
        let initial_chunk = chunk.size_for(initial_elements as u64).ok_or_else(|| {
            unsupported(format!("a sample of {initial_elements} elements overflows"))
        })?;
        let budget = PortBudget {
            allocation,
            chunk,
            element_size: described.payload.size,
            initial_chunk,
        };
        budget.check_elements(display, initial_elements as u64)?;
        Ok(budget)
    }

    /// Checks the data segments that can be alive at once after a sample of `elements` payload elements is loaned:
    /// a slice publisher's segments grow to hold the largest sample loaned, and older segments stay while samples
    /// in them are referenced (see [`PortAllocation::check_data_segments`]).
    pub(crate) fn check_elements(&self, display: &str, elements: u64) -> Result<(), Error> {
        let chunk = self.chunk.size_for(elements).unwrap_or(u64::MAX);
        self.allocation
            .check_data_segments(self.initial_chunk, chunk, self.chunk.alignment)
            .map(|_| ())
            .map_err(|e| Error::Unsupported {
                service: display.to_string(),
                reason: format!("a sample of {elements} elements: {e}"),
            })
    }

    /// Checks the data segments before a slice payload of `payload_len` bytes is loaned; fixed-size payloads, and
    /// samples that fit the first segment's chunks, were checked when the publisher was created.
    pub(crate) fn check_payload(&self, display: &str, payload_len: usize) -> Result<(), Error> {
        if self.allocation.segments == 1 || self.element_size == 0 {
            return Ok(());
        }
        let elements = payload_len as u64 / self.element_size;
        if self
            .chunk
            .size_for(elements)
            .is_some_and(|c| c <= self.initial_chunk)
        {
            return Ok(());
        }
        self.check_elements(display, elements)
    }
}

type Factory<P, H> =
    iceoryx2::service::port_factory::publish_subscribe::PortFactory<ipc::Service, P, H>;

/// What to ask for if the recorder or replayer has to create a service rather than open it.
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub enum Create {
    /// The recorder: room for its subscriber buffer.
    Recorder { buffer: usize },
    /// The replayer: the recorded settings (zero values are left at iceoryx2's defaults).
    Replay(ServiceSettings),
}

/// Opens a publish-subscribe service, or creates it with `$create` from the node `$node` if it doesn't exist. `$make`
/// is a closure that returns a fresh builder; the same code serves typed, slice and runtime-typed builders. Only an
/// open error saying the service doesn't exist leads to a create, and only a create error saying it already exists
/// (someone created it in between) leads to a second open; every other error is returned as it is. Before the
/// create, the settings it would give the service (the node's configured defaults where `$create` gives none) are
/// checked against the caps ([`check_before_create`]).
macro_rules! open_or_create {
    ($node:expr, $make:expr, $display:expr, $create:expr) => {{
        use iceoryx2::service::builder::publish_subscribe::{
            PublishSubscribeCreateError as CreateError, PublishSubscribeOpenError as OpenError,
        };
        let make = $make;
        match make().open() {
            Ok(s) => Ok(s),
            Err(OpenError::DoesNotExist) => {
                $crate::typed::check_before_create($node, $display, $create)?;
                let mut b = make();
                match $create {
                    $crate::typed::Create::Recorder { buffer } => {
                        b = b.subscriber_max_buffer_size(buffer)
                    }
                    $crate::typed::Create::Replay(s) => {
                        // Validated settings (layout::ServiceSettings::validate) fit in usize on every supported
                        // platform; zero means unknown and keeps iceoryx2's default.
                        let n = |v: u64| usize::try_from(v).unwrap_or(usize::MAX);
                        b = b
                            .history_size(n(s.history_size))
                            .enable_safe_overflow(s.safe_overflow);
                        if s.subscriber_max_buffer_size > 0 {
                            b = b.subscriber_max_buffer_size(n(s.subscriber_max_buffer_size));
                        }
                        if s.subscriber_max_borrowed_samples > 0 {
                            b = b.subscriber_max_borrowed_samples(n(
                                s.subscriber_max_borrowed_samples
                            ));
                        }
                        if s.max_publishers > 0 {
                            b = b.max_publishers(n(s.max_publishers));
                        }
                        if s.max_subscribers > 0 {
                            b = b.max_subscribers(n(s.max_subscribers));
                        }
                        if s.max_nodes > 0 {
                            b = b.max_nodes(n(s.max_nodes));
                        }
                    }
                }
                match b.create() {
                    Ok(s) => Ok(s),
                    Err(CreateError::AlreadyExists) => make()
                        .open()
                        .map_err(|e| $crate::recorder::open_error($display, e)),
                    Err(e) => Err($crate::recorder::create_error($display, e)),
                }
            }
            Err(e) => Err($crate::recorder::open_error($display, e)),
        }
    }};
}
#[cfg(feature = "dynamic")]
pub(crate) use open_or_create;

/// Called with each received message's user header, payload bytes and publisher id.
#[doc(hidden)]
pub type Visit<'a, H> = dyn FnMut(&H, &[u8], u128) -> Result<(), Error> + 'a;

mod sealed {
    pub trait Sealed {}
    impl<T> Sealed for T {}
    impl<T> Sealed for [T] {}
}

/// A user header the typed recorder accepts: any iceoryx2 header type whose bytes can be viewed safely.
pub trait RecordableHeader: Debug + ZeroCopySend + IntoBytes + Immutable + 'static {}
impl<H: Debug + ZeroCopySend + IntoBytes + Immutable + 'static> RecordableHeader for H {}

/// A user header the typed replayer accepts: it can be rebuilt from bytes.
pub trait ReplayableHeader: RecordableHeader + FromBytes + Default {}
impl<H: RecordableHeader + FromBytes + Default> ReplayableHeader for H {}

/// A payload the typed recorder accepts: `T` or a slice `[T]`, where `T` is an iceoryx2 payload type (it implements
/// `ZeroCopySend`) whose bytes can be viewed without `unsafe` (`zerocopy::IntoBytes + Immutable`, so it has no
/// padding). Types with padding can be recorded with the `dynamic` recorder instead.
pub trait Recordable: sealed::Sealed + Debug + IceoryxSend + 'static {
    /// The payload type details iceoryx2 derives for `Self` (without an alignment override).
    #[doc(hidden)]
    fn payload_type() -> TypeInfo;

    #[doc(hidden)]
    fn open_service<H: RecordableHeader>(
        node: &Node<ipc::Service>,
        name: &ServiceName,
        display: &str,
        create: Create,
    ) -> Result<Factory<Self, H>, Error>;

    #[doc(hidden)]
    fn drain<H: RecordableHeader>(
        subscriber: &Subscriber<ipc::Service, Self, H>,
        max: usize,
        f: &mut Visit<'_, H>,
    ) -> Result<usize, Error>;
}

/// A payload the typed replayer accepts: a [`Recordable`] type that can also be rebuilt from bytes
/// (`zerocopy::FromBytes`).
pub trait Replayable: Recordable {
    /// Checks what the replay publisher of a service with the settings and types `cfg` (as opened, or read from an
    /// existing service's details) will make iceoryx2 allocate (see [`PortBudget`]).
    #[doc(hidden)]
    fn budget(
        node: &Node<ipc::Service>,
        cfg: &PubSubConfig,
        display: &str,
    ) -> Result<PortBudget, Error>;

    /// Creates the replay publisher, after checking what it will allocate (see [`PortBudget`]).
    #[doc(hidden)]
    fn publisher<H: ReplayableHeader>(
        node: &Node<ipc::Service>,
        factory: &Factory<Self, H>,
        display: &str,
    ) -> Result<(Publisher<ipc::Service, Self, H>, PortBudget), Error>;

    #[doc(hidden)]
    fn send<H: ReplayableHeader>(
        publisher: &Publisher<ipc::Service, Self, H>,
        user_header: &[u8],
        payload: &[u8],
    ) -> Result<(), Error>;
}

fn header_from<H: ReplayableHeader>(bytes: &[u8]) -> Result<H, Error> {
    H::read_from_bytes(bytes).map_err(|_| {
        Error::Usage(format!(
            "recorded user header has {} bytes, the registered type has {}",
            bytes.len(),
            size_of::<H>()
        ))
    })
}

impl<T: Debug + ZeroCopySend + IntoBytes + Immutable + 'static> Recordable for T {
    fn payload_type() -> TypeInfo {
        type_info(&TypeDetail::new::<T>(IoxVariant::FixedSize))
    }

    fn open_service<H: RecordableHeader>(
        node: &Node<ipc::Service>,
        name: &ServiceName,
        display: &str,
        create: Create,
    ) -> Result<Factory<Self, H>, Error> {
        open_or_create!(
            node,
            || node
                .service_builder(name)
                .publish_subscribe::<T>()
                .user_header::<H>(),
            display,
            create
        )
    }

    fn drain<H: RecordableHeader>(
        subscriber: &Subscriber<ipc::Service, T, H>,
        max: usize,
        f: &mut Visit<'_, H>,
    ) -> Result<usize, Error> {
        let mut n = 0;
        while n < max {
            let Some(sample) = subscriber.receive().map_err(|e| Error::iox("receive", e))? else {
                break;
            };
            f(
                sample.user_header(),
                sample.payload().as_bytes(),
                sample.origin().value(),
            )?;
            n += 1;
        }
        Ok(n)
    }
}

impl<T: Debug + ZeroCopySend + IntoBytes + Immutable + 'static> Recordable for [T] {
    fn payload_type() -> TypeInfo {
        type_info(&TypeDetail::new::<T>(IoxVariant::Dynamic))
    }

    fn open_service<H: RecordableHeader>(
        node: &Node<ipc::Service>,
        name: &ServiceName,
        display: &str,
        create: Create,
    ) -> Result<Factory<Self, H>, Error> {
        open_or_create!(
            node,
            || node
                .service_builder(name)
                .publish_subscribe::<[T]>()
                .user_header::<H>(),
            display,
            create
        )
    }

    fn drain<H: RecordableHeader>(
        subscriber: &Subscriber<ipc::Service, [T], H>,
        max: usize,
        f: &mut Visit<'_, H>,
    ) -> Result<usize, Error> {
        let mut n = 0;
        while n < max {
            let Some(sample) = subscriber.receive().map_err(|e| Error::iox("receive", e))? else {
                break;
            };
            f(
                sample.user_header(),
                sample.payload().as_bytes(),
                sample.origin().value(),
            )?;
            n += 1;
        }
        Ok(n)
    }
}

impl<T: Debug + ZeroCopySend + IntoBytes + FromBytes + Immutable + 'static> Replayable for T {
    fn budget(
        node: &Node<ipc::Service>,
        cfg: &PubSubConfig,
        display: &str,
    ) -> Result<PortBudget, Error> {
        // A fixed-size publisher takes the configured default strategy (static unless a configuration file says
        // otherwise); iceoryx2 offers no setter for it.
        PortBudget::check(
            display,
            cfg,
            PUBLISHER_MAX_LOANED,
            default_strategy_grows(node),
            1,
        )
    }

    fn publisher<H: ReplayableHeader>(
        node: &Node<ipc::Service>,
        factory: &Factory<T, H>,
        display: &str,
    ) -> Result<(Publisher<ipc::Service, T, H>, PortBudget), Error> {
        let budget = Self::budget(node, factory.static_config(), display)?;
        let publisher = factory
            .publisher_builder()
            .max_loaned_samples(PUBLISHER_MAX_LOANED)
            .create()
            .map_err(|e| Error::iox("create publisher", e))?;
        Ok((publisher, budget))
    }

    fn send<H: ReplayableHeader>(
        publisher: &Publisher<ipc::Service, T, H>,
        user_header: &[u8],
        payload: &[u8],
    ) -> Result<(), Error> {
        let value = T::read_from_bytes(payload).map_err(|_| {
            Error::Usage(format!(
                "recorded payload has {} bytes, the registered type has {}",
                payload.len(),
                size_of::<T>()
            ))
        })?;
        let header = header_from::<H>(user_header)?;
        let mut sample = publisher.loan_uninit().map_err(|e| Error::iox("loan", e))?;
        *sample.user_header_mut() = header;
        sample
            .write_payload(value)
            .send()
            .map_err(|e| Error::iox("send", e))?;
        Ok(())
    }
}

impl<T: Debug + ZeroCopySend + IntoBytes + FromBytes + Immutable + Copy + 'static> Replayable
    for [T]
{
    fn budget(
        _node: &Node<ipc::Service>,
        cfg: &PubSubConfig,
        display: &str,
    ) -> Result<PortBudget, Error> {
        PortBudget::check(
            display,
            cfg,
            PUBLISHER_MAX_LOANED,
            true,
            initial_slice_len(cfg),
        )
    }

    fn publisher<H: ReplayableHeader>(
        node: &Node<ipc::Service>,
        factory: &Factory<[T], H>,
        display: &str,
    ) -> Result<(Publisher<ipc::Service, [T], H>, PortBudget), Error> {
        let initial = initial_slice_len(factory.static_config());
        let budget = Self::budget(node, factory.static_config(), display)?;
        let publisher = factory
            .publisher_builder()
            .max_loaned_samples(PUBLISHER_MAX_LOANED)
            .initial_max_slice_len(initial)
            .allocation_strategy(AllocationStrategy::PowerOfTwo)
            .create()
            .map_err(|e| Error::iox("create publisher", e))?;
        Ok((publisher, budget))
    }

    fn send<H: ReplayableHeader>(
        publisher: &Publisher<ipc::Service, [T], H>,
        user_header: &[u8],
        payload: &[u8],
    ) -> Result<(), Error> {
        let size = size_of::<T>();
        if size == 0 || !payload.len().is_multiple_of(size) {
            return Err(Error::Usage(format!(
                "recorded payload of {} bytes is not a whole number of {size}-byte elements",
                payload.len()
            )));
        }
        let header = header_from::<H>(user_header)?;
        let mut sample = publisher
            .loan_slice_uninit(payload.len() / size)
            .map_err(|e| Error::iox("loan", e))?;
        *sample.user_header_mut() = header;
        let mut chunks = payload.chunks_exact(size);
        sample
            .write_from_fn(|_| {
                // Every chunk has exactly `size` bytes, so this conversion cannot fail.
                T::read_from_bytes(chunks.next().unwrap_or_default())
                    .unwrap_or_else(|_| T::new_zeroed())
            })
            .send()
            .map_err(|e| Error::iox("send", e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iceoryx2::config::Config;

    fn name(what: &str) -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        format!(
            "tt-typed-unit/{}-{}/{what}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// A node whose configured publish-subscribe defaults are changed by `f`.
    fn node_with(f: impl FnOnce(&mut Config)) -> Node<ipc::Service> {
        let mut config = Config::global_config().clone();
        f(&mut config);
        NodeBuilder::new()
            .config(&config)
            .signal_handling_mode(SignalHandlingMode::Disabled)
            .create::<ipc::Service>()
            .unwrap()
    }

    /// Review round 7: a service that doesn't exist yet is created with the node's configured defaults where the
    /// recording (or the recorder) gives no value; a default over a cap (`max_publishers = 40000`) is refused before
    /// `create()`, which would size the new service's shared configuration by it, and the service is not created.
    #[test]
    fn configured_defaults_over_a_cap_are_refused_before_create() {
        let node = node_with(|c| c.defaults.publish_subscribe.max_publishers = 40_000);
        for create in [
            Create::Replay(ServiceSettings::UNKNOWN),
            Create::Recorder { buffer: 16 },
        ] {
            let display = name("defaults-over-cap");
            let service = ServiceName::new(&display).unwrap();
            let err = <u64 as Recordable>::open_service::<()>(&node, &service, &display, create)
                .map(drop)
                .unwrap_err();
            assert!(
                matches!(&err, Error::Unsupported { reason, .. }
                    if reason.contains("would be created with") && reason.contains("max_publishers is 40000")),
                "{create:?}: {err}"
            );
            let open = node
                .service_builder(&service)
                .publish_subscribe::<u64>()
                .open();
            assert!(
                matches!(
                    open,
                    Err(iceoryx2::service::builder::publish_subscribe::PublishSubscribeOpenError::DoesNotExist)
                ),
                "{create:?}: the service was created"
            );
        }
        // A recorded value replaces the default, so the same node creates a service with a recorded max_publishers.
        let display = name("recorded-replaces-default");
        let recorded = ServiceSettings {
            max_publishers: 4,
            ..ServiceSettings::UNKNOWN
        };
        let factory = <u64 as Recordable>::open_service::<()>(
            &node,
            &ServiceName::new(&display).unwrap(),
            &display,
            Create::Replay(recorded),
        )
        .unwrap();
        assert_eq!(factory.static_config().max_publishers(), 4);
    }

    /// The settings checked before `create()` are the ones iceoryx2 then gives the service: unusual configured
    /// defaults (zeros among them, which iceoryx2 raises to 1), with and without recorded values.
    #[test]
    fn settings_to_create_are_what_iceoryx2_creates() {
        let node = node_with(|c| {
            let d = &mut c.defaults.publish_subscribe;
            d.max_publishers = 0;
            d.max_subscribers = 7;
            d.max_nodes = 5;
            d.publisher_history_size = 3;
            d.subscriber_max_buffer_size = 0;
            d.subscriber_max_borrowed_samples = 4;
            d.enable_safe_overflow = false;
        });
        let recorded = ServiceSettings {
            history_size: 2,
            subscriber_max_buffer_size: 9,
            subscriber_max_borrowed_samples: 0,
            max_publishers: 0,
            max_subscribers: 6,
            max_nodes: 0,
            safe_overflow: true,
        };
        for create in [
            Create::Replay(ServiceSettings::UNKNOWN),
            Create::Replay(recorded),
            Create::Recorder { buffer: 11 },
        ] {
            let display = name("settings-to-create");
            let expected = settings_to_create(&node, create);
            let factory = <u64 as Recordable>::open_service::<()>(
                &node,
                &ServiceName::new(&display).unwrap(),
                &display,
                create,
            )
            .unwrap();
            let unknown = Version {
                major: 0,
                minor: 0,
                patch: 0,
            };
            let got = describe(factory.static_config(), unknown, 0).settings;
            assert_eq!(got, expected, "{create:?}");
        }
    }
}
