//! Services whose Rust types are only known at run time (feature `dynamic`).
//!
//! This is what `iox2 service record` and `iox2 service replay` do: read a running service's type details (type
//! name, size, alignment, fixed or slice) and handle its messages as raw bytes. iceoryx2 0.10 offers this only
//! through API it marks internal and hides from its documentation:
//!
//! - `publish_subscribe::Builder::__internal_set_payload_type_details` and
//!   `__internal_set_user_header_type_details` (both `unsafe`), with the marker types `CustomPayloadMarker` and
//!   `CustomHeaderMarker`, to open a service with type details given at run time;
//! - `Publisher::loan_custom_payload` (`unsafe`) to loan a sample of a given number of elements;
//! - `TypeDetail::__internal_new_from_parts` to build type details from a recording.
//!
//! Upstream may change any of these in a minor release; this module is the only place that uses them, so that a
//! change is easy to follow. It is also the only module of the crate with `unsafe` code: every block is listed
//! with its reasoning below and in the pull request that added it. The typed [`recorder`](crate::recorder) and
//! [`replayer`](crate::replayer) paths need none.
//!
//! Recording copies the bytes exactly as they are in shared memory, including padding inside the user's types;
//! replay writes them back unchanged.

use iceoryx2::port::listener::Listener;
use iceoryx2::port::publisher::Publisher;
use iceoryx2::port::subscriber::Subscriber;
use iceoryx2::prelude::*;
use iceoryx2::service::marker::{CustomHeaderMarker, CustomPayloadMarker};
use iceoryx2::service::port_factory::PortFactory as _;
use iceoryx2::service::port_factory::publish_subscribe::PortFactory;
use iceoryx2::service::static_config::message_type_details::{
    TypeDetail, TypeVariant as IoxVariant,
};

use crate::Error;
use crate::layout::{CounterField, Descriptor, TypeInfo, TypeVariant};
use crate::recorder::{
    Described, Message, Source, SourceFactory, describe, open_listener, service_name,
    service_version, type_info,
};
use crate::replayer::Sink;
use crate::typed::{Create, open_or_create};

type RawFactory = PortFactory<ipc::Service, [CustomPayloadMarker], CustomHeaderMarker>;

/// A service recorded by name, with the types its publishers created it with (feature `dynamic`).
///
/// The service must exist before it can be recorded, because its types are read from it; until it does, the
/// recorder retries every 100 ms.
pub struct DynamicService {
    name: String,
    venue_ts_offset: Option<u32>,
    counter: Option<CounterField>,
    notify: Option<String>,
}

impl core::fmt::Debug for DynamicService {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DynamicService")
            .field("name", &self.name)
            .field("venue_ts_offset", &self.venue_ts_offset)
            .field("counter", &self.counter)
            .field("notify", &self.notify)
            .finish()
    }
}

impl DynamicService {
    /// The service named `name`.
    pub fn new(name: impl Into<String>) -> Self {
        DynamicService {
            name: name.into(),
            venue_ts_offset: None,
            counter: None,
            notify: None,
        }
    }

    /// Reads `venue_ts_ns` as an unsigned little-endian 64-bit integer at this byte offset in the user header.
    /// Messages whose header is too short get 0.
    pub fn venue_ts_offset(mut self, offset: u32) -> Self {
        self.venue_ts_offset = Some(offset);
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

/// Builds an iceoryx2 `TypeDetail` from a recorded one.
pub(crate) fn type_detail(info: &TypeInfo) -> Result<TypeDetail, Error> {
    let n = |v: u64, what: &str| {
        usize::try_from(v)
            .map_err(|_| Error::Usage(format!("recorded {what} {v} does not fit this platform")))
    };
    TypeDetail::__internal_new_from_parts(
        match info.variant {
            TypeVariant::FixedSize => IoxVariant::FixedSize,
            TypeVariant::Dynamic => IoxVariant::Dynamic,
        },
        &info.name,
        n(info.size, "type size")?,
        n(info.alignment, "type alignment")?,
    )
    .map_err(|e| {
        Error::Usage(format!(
            "recorded type name \"{}\" is not valid for iceoryx2: {e:?}",
            info.name
        ))
    })
}

/// Opens (or, with `create`, creates) a service with type details given at run time.
fn open_raw(
    node: &Node<ipc::Service>,
    name: &ServiceName,
    display: &str,
    payload: &TypeDetail,
    user_header: &TypeDetail,
    create: Option<Create>,
) -> Result<RawFactory, Error> {
    let make = || {
        let builder = node
            .service_builder(name)
            .publish_subscribe::<[CustomPayloadMarker]>()
            .user_header::<CustomHeaderMarker>();
        // SAFETY: (unsafe block 1) The type details are either read from the running service (recording) or
        // recorded from it (replay), so they describe the memory layout its other ports use; iceoryx2 checks them
        // against the existing service on open. This module only ever reads and writes those samples as plain
        // bytes, within the sizes the same details give (see `DynamicSource::drain` and `DynamicSink::send`).
        unsafe {
            builder
                .__internal_set_payload_type_details(payload)
                .__internal_set_user_header_type_details(user_header)
        }
    };
    match create {
        Some(create) => open_or_create!(make, display, create),
        None => make()
            .open()
            .map_err(|e| crate::recorder::incompatible_or(display, e)),
    }
}

impl SourceFactory for DynamicService {
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
        let details =
            ipc::Service::details(&name, node.config(), MessagingPattern::PublishSubscribe)
                .map_err(|e| Error::iox(&format!("read details of \"{}\"", self.name), e))?;
        let Some(details) = details else {
            return Ok(None);
        };
        let types = *details
            .static_details
            .publish_subscribe()
            .message_type_details();
        let factory = match open_raw(
            node,
            &name,
            &self.name,
            &types.payload,
            &types.user_header,
            None,
        ) {
            Ok(f) => f,
            // It went away between the two calls; try again later.
            Err(Error::Iceoryx2(text)) if text.contains("DoesNotExist") => return Ok(None),
            Err(e) => return Err(e),
        };
        let cfg = factory.static_config();
        let buffer = buffer_size.min(cfg.subscriber_max_buffer_size()).max(1);
        let subscriber = factory
            .subscriber_builder()
            .buffer_size(buffer)
            .create()
            .map_err(|e| Error::iox("create subscriber", e))?;
        let described = describe(cfg, service_version(node, &name), buffer);
        Ok(Some(Box::new(DynamicSource {
            user_header_len: types.user_header.size(),
            subscriber,
            venue_ts_offset: self.venue_ts_offset,
            listener: open_listener(node, self.notify.as_deref())?,
            described,
        })))
    }
}

struct DynamicSource {
    subscriber: Subscriber<ipc::Service, [CustomPayloadMarker], CustomHeaderMarker>,
    user_header_len: usize,
    venue_ts_offset: Option<u32>,
    listener: Option<Listener<ipc::Service>>,
    described: Described,
}

impl Source for DynamicSource {
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
        let mut n = 0;
        while n < max {
            let Some(sample) = self
                .subscriber
                .receive()
                .map_err(|e| Error::iox("receive", e))?
            else {
                break;
            };
            let payload = sample.payload();
            // SAFETY: (unsafe block 2) For `[CustomPayloadMarker]` (a one-byte marker), iceoryx2 sizes the payload
            // slice in bytes: number of elements times the element size from the type details. The memory is the
            // sample's payload in shared memory, mapped read-only for this subscriber while `sample` lives, and
            // this slice doesn't outlive `sample`. Bytes written by another process are initialized as far as this
            // process can tell (as for any shared mapping).
            let payload = unsafe {
                core::slice::from_raw_parts(payload.as_ptr().cast::<u8>(), payload.len())
            };
            let header: *const CustomHeaderMarker = sample.user_header();
            // SAFETY: (unsafe block 3) `CustomHeaderMarker` has size 0; the sample's real user header starts at
            // this address and is `user_header_len` bytes long, the size from the service's type details, which
            // iceoryx2 reserves in every sample's chunk. It lives as long as `sample`, and the slice doesn't
            // outlive it.
            let user_header =
                unsafe { core::slice::from_raw_parts(header.cast::<u8>(), self.user_header_len) };
            let venue_ts_ns = self.venue_ts_offset.map_or(0, |offset| {
                let start = offset as usize;
                user_header
                    .get(start..start.saturating_add(8))
                    .and_then(|b| b.try_into().ok())
                    .map_or(0, u64::from_le_bytes)
            });
            f(Message {
                user_header,
                payload,
                origin: sample.origin().value(),
                venue_ts_ns,
            })?;
            n += 1;
        }
        Ok(n)
    }
}

/// Opens a publisher on `target` with the recorded type details, creating the service with the recorded settings
/// if it doesn't exist.
pub(crate) fn sink(
    node: &Node<ipc::Service>,
    target: &str,
    descriptor: &Descriptor,
) -> Result<Box<dyn Sink>, Error> {
    let name = service_name(target)?;
    let payload = type_detail(&descriptor.payload)?;
    let user_header = type_detail(&descriptor.user_header)?;
    if descriptor.payload.size == 0 && descriptor.payload.variant == TypeVariant::Dynamic {
        return Err(Error::Usage(format!(
            "service \"{}\" has a slice payload of zero-sized elements, which can't be replayed",
            descriptor.service_name
        )));
    }
    let factory = open_raw(
        node,
        &name,
        target,
        &payload,
        &user_header,
        Some(Create::Replay(descriptor.settings)),
    )?;
    let element = payload.size();
    let publisher = match payload.variant() {
        IoxVariant::FixedSize => factory.publisher_builder().create(),
        IoxVariant::Dynamic => factory
            .publisher_builder()
            .initial_max_slice_len((4096 / element.max(1)).max(1))
            .allocation_strategy(AllocationStrategy::PowerOfTwo)
            .create(),
    }
    .map_err(|e| Error::iox("create publisher", e))?;
    Ok(Box::new(DynamicSink {
        variant: payload.variant(),
        element,
        user_header_len: user_header.size(),
        factory,
        publisher,
    }))
}

struct DynamicSink {
    factory: RawFactory,
    publisher: Publisher<ipc::Service, [CustomPayloadMarker], CustomHeaderMarker>,
    variant: IoxVariant,
    element: usize,
    user_header_len: usize,
}

impl Sink for DynamicSink {
    fn send(&mut self, user_header: &[u8], payload: &[u8]) -> Result<(), Error> {
        if user_header.len() != self.user_header_len {
            return Err(Error::Usage(format!(
                "recorded user header has {} bytes, the service's has {}",
                user_header.len(),
                self.user_header_len
            )));
        }
        let elements = match self.variant {
            IoxVariant::FixedSize if payload.len() == self.element => 1,
            IoxVariant::Dynamic
                if self.element > 0 && payload.len().is_multiple_of(self.element) =>
            {
                payload.len() / self.element
            }
            _ => {
                return Err(Error::Usage(format!(
                    "recorded payload of {} bytes doesn't fit the service's {}-byte {:?} type",
                    payload.len(),
                    self.element,
                    self.variant
                )));
            }
        };
        // SAFETY: (unsafe block 4) `loan_custom_payload` requires: a slice length other than 1 only for a dynamic
        // payload (checked above), and that the sample be treated as `elements * element size` bytes, which is
        // `payload.len()`, exactly what is written below.
        let mut sample = unsafe { self.publisher.loan_custom_payload(elements) }
            .map_err(|e| Error::iox("loan", e))?;
        let destination = sample.payload_mut();
        if destination.len() != payload.len() {
            return Err(Error::Iceoryx2(format!(
                "loaned {} bytes for a {}-byte payload",
                destination.len(),
                payload.len()
            )));
        }
        // SAFETY: (unsafe block 5) `destination` is the loaned payload, `payload.len()` bytes long (checked just
        // above), writable and exclusively ours until `send`; `MaybeUninit<CustomPayloadMarker>` has the size and
        // alignment of `u8`. The source is a separate buffer, so the ranges don't overlap.
        unsafe {
            core::ptr::copy_nonoverlapping(
                payload.as_ptr(),
                destination.as_mut_ptr().cast::<u8>(),
                payload.len(),
            );
        }
        let header: *mut CustomHeaderMarker = sample.user_header_mut();
        // SAFETY: (unsafe block 6) The sample's user header area starts at the zero-sized marker's address and is
        // `user_header_len` bytes long, the size in the service's type details (checked equal to
        // `user_header.len()` above). It is exclusively ours until `send`, and doesn't overlap `user_header`.
        unsafe {
            core::ptr::copy_nonoverlapping(
                user_header.as_ptr(),
                header.cast::<u8>(),
                user_header.len(),
            );
        }
        // SAFETY: (unsafe block 7) Every byte of the payload and user header was written above.
        let sample = unsafe { sample.assume_init() };
        sample.send().map_err(|e| Error::iox("send", e))?;
        Ok(())
    }

    fn subscribers(&self) -> usize {
        self.factory.dynamic_config().number_of_subscribers()
    }
}

/// iceoryx2's sample header type detail, as this build of iceoryx2 defines it (for files written by
/// [`upstream`](crate::upstream)).
pub(crate) fn system_header_detail() -> TypeDetail {
    TypeDetail::new::<iceoryx2::service::header::publish_subscribe::Header>(IoxVariant::FixedSize)
}

pub(crate) fn type_info_of(detail: &TypeDetail) -> TypeInfo {
    type_info(detail)
}
