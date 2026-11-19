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

/// Checks what the recorder's subscriber of the service `display` will make iceoryx2 allocate, with the service's
/// settings in force (`cfg`), before it is created: it connects to every publisher, sized by the samples each
/// preallocates (assumed to use the node's default loaned samples) and their data segments (255 for a slice service,
/// whose publishers grow their segments).
pub(crate) fn check_subscriber(
    node: &Node<ipc::Service>,
    display: &str,
    cfg: &PubSubConfig,
) -> Result<(), Error> {
    let growing = cfg.message_type_details().payload.variant() == IoxVariant::Dynamic
        || default_strategy_grows(node);
    let loaned = node
        .config()
        .defaults
        .publish_subscribe
        .publisher_max_loaned_samples;
    PortBudget::check(display, cfg, loaned, growing, None).map(|_| ())
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

/// What a port of an opened (or created) service will make iceoryx2 allocate, checked before the port is created;
/// see [memory a port may make iceoryx2 allocate](crate::layout#memory-a-port-may-make-iceoryx2-allocate).
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct PortBudget {
    allocation: PortAllocation,
    chunk: ChunkLayout,
    element_size: u64,
}

impl PortBudget {
    /// Checks the settings and types in force of the service `display` (`cfg`, from the port factory: what iceoryx2
    /// opened or created, defaults included) for a port. A publisher has `max_loaned` loaned samples, a growing
    /// allocation strategy (255 data segments) or a static one, and starts with `initial_elements` (its data segment
    /// is checked too); for a subscriber pass `None` (it doesn't allocate the publishers' data segments) and whether
    /// the publishers may use a growing strategy.
    pub(crate) fn check(
        display: &str,
        cfg: &PubSubConfig,
        max_loaned: usize,
        growing: bool,
        initial_elements: Option<usize>,
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
            layout::port_allocation(&described.settings, max_loaned as u64, growing)
                .map_err(|e| unsupported(format!("the service's settings in force: {e}")))?;
        let budget = PortBudget {
            allocation,
            chunk,
            element_size: described.payload.size,
        };
        if let Some(n) = initial_elements {
            budget.check_elements(display, n as u64)?;
        }
        Ok(budget)
    }

    /// Checks the data segment for a sample of `elements` payload elements (a slice publisher's segment grows to
    /// hold the largest sample loaned).
    pub(crate) fn check_elements(&self, display: &str, elements: u64) -> Result<(), Error> {
        let chunk = self.chunk.size_for(elements).unwrap_or(u64::MAX);
        self.allocation
            .check_data_segment(chunk, self.chunk.alignment)
            .map(|_| ())
            .map_err(|e| Error::Unsupported {
                service: display.to_string(),
                reason: format!("a sample of {elements} elements: {e}"),
            })
    }

    /// Checks the data segment before a slice payload of `payload_len` bytes is loaned; fixed-size payloads were
    /// checked when the publisher was created.
    pub(crate) fn check_payload(&self, display: &str, payload_len: usize) -> Result<(), Error> {
        if self.allocation.segments == 1 || self.element_size == 0 {
            return Ok(());
        }
        self.check_elements(display, payload_len as u64 / self.element_size)
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

/// Opens a publish-subscribe service, or creates it with `$create` if it doesn't exist. `$make` is a closure that
/// returns a fresh builder; the same code serves typed, slice and runtime-typed builders. Only an open error saying
/// the service doesn't exist leads to a create, and only a create error saying it already exists (someone created
/// it in between) leads to a second open; every other error is returned as it is.
macro_rules! open_or_create {
    ($make:expr, $display:expr, $create:expr) => {{
        use iceoryx2::service::builder::publish_subscribe::{
            PublishSubscribeCreateError as CreateError, PublishSubscribeOpenError as OpenError,
        };
        let make = $make;
        match make().open() {
            Ok(s) => Ok(s),
            Err(OpenError::DoesNotExist) => {
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
    fn publisher<H: ReplayableHeader>(
        node: &Node<ipc::Service>,
        factory: &Factory<T, H>,
        display: &str,
    ) -> Result<(Publisher<ipc::Service, T, H>, PortBudget), Error> {
        // A fixed-size publisher takes the configured default strategy (static unless a configuration file says
        // otherwise); iceoryx2 offers no setter for it.
        let budget = PortBudget::check(
            display,
            factory.static_config(),
            PUBLISHER_MAX_LOANED,
            default_strategy_grows(node),
            Some(1),
        )?;
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
    fn publisher<H: ReplayableHeader>(
        _node: &Node<ipc::Service>,
        factory: &Factory<[T], H>,
        display: &str,
    ) -> Result<(Publisher<ipc::Service, [T], H>, PortBudget), Error> {
        let initial = initial_slice_len(factory.static_config());
        let budget = PortBudget::check(
            display,
            factory.static_config(),
            PUBLISHER_MAX_LOANED,
            true,
            Some(initial),
        )?;
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
