//! Typed payloads: byte views through `zerocopy`, so the typed recorder and replayer need no `unsafe`.

use core::fmt::Debug;

use iceoryx2::port::publisher::Publisher;
use iceoryx2::port::subscriber::Subscriber;
use iceoryx2::prelude::*;
use zerocopy::{FromBytes, Immutable, IntoBytes};

use crate::Error;
use crate::layout::ServiceSettings;

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
    #[doc(hidden)]
    fn publisher<H: ReplayableHeader>(
        factory: &Factory<Self, H>,
    ) -> Result<Publisher<ipc::Service, Self, H>, Error>;

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
        factory: &Factory<T, H>,
    ) -> Result<Publisher<ipc::Service, T, H>, Error> {
        factory
            .publisher_builder()
            .create()
            .map_err(|e| Error::iox("create publisher", e))
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
        factory: &Factory<[T], H>,
    ) -> Result<Publisher<ipc::Service, [T], H>, Error> {
        factory
            .publisher_builder()
            .initial_max_slice_len(4096 / size_of::<T>().max(1))
            .allocation_strategy(AllocationStrategy::PowerOfTwo)
            .create()
            .map_err(|e| Error::iox("create publisher", e))
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
