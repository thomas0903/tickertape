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
//! change is easy to follow. It is also the only module of the crate with `unsafe` code (the crate denies
//! `unsafe_code` everywhere else): every block is listed with its reasoning below and in the pull request that
//! added it. The typed [`recorder`](crate::recorder) and [`replayer`](crate::replayer) paths need none.
//!
//! Type details reach these setters only after [`layout::validate_types`] accepted them, whether they come from a
//! running service, a journal or a file: alignment, sizes, the chunk layout they imply and the user header's
//! variant. Replayed service settings are checked by [`layout::ServiceSettings::validate`] first too.
//!
//! Recording copies the bytes exactly as they are in shared memory, including padding inside the user's types;
//! replay writes them back unchanged.
//!
//! # Assumptions this module makes about iceoryx2
//!
//! - **The user header is reached through a zero-sized reference.** For a runtime-typed service, iceoryx2 0.10 hands
//!   out the user header only as `&CustomHeaderMarker` / `&mut CustomHeaderMarker`, a reference to a zero-sized
//!   type at the header's address, and offers no accessor that yields a pointer or slice derived from the chunk
//!   (its chunk accessors are crate-private). This module, like upstream's own `iox2 service record`, `replay`,
//!   `publish` and `subscribe`, casts that reference to a byte pointer and accesses the header's
//!   `user header size` bytes through it. The address and its provenance come from the shared-memory mapping
//!   (iceoryx2 derives the reference from the chunk's raw pointer), but the reference itself covers zero bytes.
//!   Under the Tree Borrows aliasing model, accesses outside a reference's range through a pointer derived from it
//!   are permitted; under Stacked Borrows they are not. Rust has not settled its aliasing model, so this relies on
//!   the more permissive one. A supported accessor is in the upstream requests drafted in the pull request.
//!   Headers of size 0 are never accessed.
//! - **Received bytes are initialised, because samples from this process are refused.** The recorder reads every
//!   payload and header byte as `u8`. Bytes written by another process are bytes of a shared mapping this process
//!   didn't write, and are initialised for it. A publisher in the *same* process that publishes a type with padding
//!   through iceoryx2's typed API may leave the padding bytes uninitialised in Rust's abstract machine, and reading
//!   them as `u8` would be undefined behaviour. So before either byte view is taken, the recorder reads the sample's
//!   system header (a fully written iceoryx2 struct, no padding), takes the process id from the sending node's id
//!   (iceoryx2 0.10's `ipc` service builds node ids as `UniqueSystemId`s, whose `pid` is the sender's host process
//!   id; `UniqueNodeId::pid`), and refuses the sample, as a per-service failure, if it is this process's (the
//!   recorder node's id, built the same way). Same-process publishers of such services can be recorded with the
//!   typed recorder, which only accepts types without padding.

use iceoryx2::port::listener::Listener;
use iceoryx2::port::publisher::Publisher;
use iceoryx2::port::subscriber::Subscriber;
use iceoryx2::prelude::*;
use iceoryx2::service::builder::publish_subscribe::Builder as PubSubBuilder;
use iceoryx2::service::marker::{CustomHeaderMarker, CustomPayloadMarker};
use iceoryx2::service::port_factory::PortFactory as _;
use iceoryx2::service::port_factory::publish_subscribe::PortFactory;
use iceoryx2::service::static_config::message_type_details::{
    TypeDetail, TypeVariant as IoxVariant,
};

use crate::Error;
use crate::layout::{self, CounterField, Descriptor, TypeInfo, TypeVariant};
use crate::recorder::{
    Described, Message, Opened, Source, SourceFactory, describe, not_there_yet, open_error,
    open_listener, service_name, service_version, type_info,
};
use crate::replayer::{ReplayReport, Replayer, Sink};
use crate::typed::{Create, PUBLISHER_MAX_LOANED, PortBudget, initial_slice_len, open_or_create};

type RawFactory = PortFactory<ipc::Service, [CustomPayloadMarker], CustomHeaderMarker>;
type RawBuilder = PubSubBuilder<[CustomPayloadMarker], CustomHeaderMarker, ipc::Service>;

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

    /// Reads `venue_ts_ns` as an unsigned little-endian 64-bit integer at this byte offset in the user header. The
    /// eight bytes must lie inside the service's user header, or the service is refused when it is opened.
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

/// Builds an iceoryx2 `TypeDetail` from a recorded one that passed [`layout::validate_types`].
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

/// A builder for a service with type details given at run time. The caller has validated them with
/// [`layout::validate_types`].
fn raw_builder(
    node: &Node<ipc::Service>,
    name: &ServiceName,
    payload: &TypeDetail,
    user_header: &TypeDetail,
) -> RawBuilder {
    let builder = node
        .service_builder(name)
        .publish_subscribe::<[CustomPayloadMarker]>()
        .user_header::<CustomHeaderMarker>();
    // SAFETY: (unsafe block 1) The type details passed `layout::validate_types` (power-of-two alignment of at most
    // 4096, sizes that are multiples of it, a fixed-size user header, a non-zero slice element size, and a chunk
    // layout that fits a Rust `Layout` for any element count a journal record can hold), so iceoryx2's unchecked
    // chunk arithmetic can't overflow. They are read from the running service (recording) or recorded from one
    // (replay), and iceoryx2 checks them against an existing service on open. This module only ever reads and writes
    // the samples as plain bytes, within the sizes the same details give (see `DynamicSource::drain` and
    // `DynamicSink::send`).
    unsafe {
        builder
            .__internal_set_payload_type_details(payload)
            .__internal_set_user_header_type_details(user_header)
    }
}

impl SourceFactory for DynamicService {
    fn name(&self) -> &str {
        &self.name
    }

    fn counter(&self) -> Option<CounterField> {
        self.counter
    }

    fn open(&mut self, node: &Node<ipc::Service>, buffer_size: usize) -> Result<Opened, Error> {
        let name = service_name(&self.name)?;
        let details =
            ipc::Service::details(&name, node.config(), MessagingPattern::PublishSubscribe)
                .map_err(|e| Error::iox(&format!("read details of \"{}\"", self.name), e))?;
        let Some(details) = details else {
            return Ok(Opened::NotYet);
        };
        let types = *details
            .static_details
            .publish_subscribe()
            .message_type_details();
        // The running service's type details come from shared memory another process wrote: validate them before
        // they reach iceoryx2's internal setters.
        let unsupported = |reason: String| Error::Unsupported {
            service: self.name.clone(),
            reason,
        };
        layout::validate_types(
            &type_info(&types.payload),
            &type_info(&types.user_header),
            &type_info(&types.header),
        )
        .map_err(|e| unsupported(e.to_string()))?;
        if let Some(offset) = self.venue_ts_offset
            && u64::from(offset) + 8 > types.user_header.size() as u64
        {
            return Err(unsupported(format!(
                "the venue timestamp at offset {offset} doesn't fit its {}-byte user header",
                types.user_header.size()
            )));
        }
        let factory = match raw_builder(node, &name, &types.payload, &types.user_header).open() {
            Ok(f) => f,
            // It went away between the two calls; try again later.
            Err(e) if not_there_yet(&e) => return Ok(Opened::NotYet),
            Err(e) => return Err(open_error(&self.name, e)),
        };
        let cfg = factory.static_config();
        crate::typed::check_subscriber(node, &self.name, cfg)?;
        let buffer = buffer_size.min(cfg.subscriber_max_buffer_size()).max(1);
        let subscriber = factory
            .subscriber_builder()
            .buffer_size(buffer)
            .create()
            .map_err(|e| Error::iox("create subscriber", e))?;
        let described = describe(cfg, service_version(node, &name), buffer);
        let own_pid = node
            .id()
            .pid::<ipc::Service>()
            .map_err(|e| Error::iox("read the recorder node's process id", e))?;
        Ok(Opened::Ready(Box::new(DynamicSource {
            name: self.name.clone(),
            own_pid: i64::from(own_pid.value()),
            user_header_len: types.user_header.size(),
            subscriber,
            venue_ts_offset: self.venue_ts_offset,
            listener: open_listener(node, self.notify.as_deref())?,
            described,
        })))
    }
}

struct DynamicSource {
    name: String,
    /// This process's id, as iceoryx2 puts it into node ids.
    own_pid: i64,
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
            // A sample from this process may hold uninitialised padding (module docs): refuse it before taking any
            // byte view. The system header is written in full by iceoryx2's publisher.
            let origin_pid = sample
                .header()
                .node_id()
                .pid::<ipc::Service>()
                .map(|p| i64::from(p.value()));
            if origin_pid.as_ref().is_ok_and(|&p| p == self.own_pid) {
                return Err(Error::Unsupported {
                    service: self.name.clone(),
                    reason: format!(
                        "a publisher in the recorder's own process (pid {}) sent a sample; the recorder by name \
                         refuses those, because padding a same-process publisher left uninitialised can't be read \
                         safely (record such services with the typed recorder, or from another process)",
                        self.own_pid
                    ),
                });
            }
            let payload = sample.payload();
            // SAFETY: (unsafe block 2) For `[CustomPayloadMarker]` (a one-byte marker), iceoryx2 sizes the payload
            // slice in bytes: number of elements times the element size from the type details. The memory is the
            // sample's payload in shared memory, mapped read-only for this subscriber while `sample` lives, and
            // this slice doesn't outlive `sample`. Bytes written by another process are initialised for this one,
            // and samples from this process were refused just above (module docs).
            let payload = unsafe {
                core::slice::from_raw_parts(payload.as_ptr().cast::<u8>(), payload.len())
            };
            let user_header: &[u8] = if self.user_header_len == 0 {
                &[]
            } else {
                let header: *const CustomHeaderMarker = sample.user_header();
                // SAFETY: (unsafe block 3) iceoryx2 reserves `user_header_len` bytes (the size in the service's type
                // details, validated when the service was opened) for the user header in every sample's chunk,
                // starting at this address, initialised by the publisher (another process: samples from this one
                // were refused above), and alive as long as `sample`; the slice
                // doesn't outlive it. The pointer comes from a reference to the zero-sized `CustomHeaderMarker`,
                // which iceoryx2 derives from the chunk pointer; accessing the bytes beyond it relies on the Tree
                // Borrows aliasing model, as the module docs explain, because iceoryx2 0.10 has no accessor that
                // yields a pointer derived from the chunk itself. Upstream's own record and subscribe tools do the
                // same.
                unsafe { core::slice::from_raw_parts(header.cast::<u8>(), self.user_header_len) }
            };
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

impl Replayer {
    /// Replays like [`Replayer::run`], and replays every service not registered with
    /// [`ReplayerBuilder::typed`](crate::replayer::ReplayerBuilder::typed) by its recorded type details, as
    /// `iox2 service replay` does: the recorded bytes are published under the recorded type name.
    ///
    /// The type details and settings are validated before they reach iceoryx2, and every message is checked against
    /// them (sizes, element counts), so a damaged or hostile journal can't make this function access memory out of
    /// bounds. What can't be checked is whether the bytes are valid *values* of the advertised types.
    ///
    /// # Safety
    ///
    /// The payload and user header of every replayed message must be a valid value of the type its descriptor
    /// advertises, for every subscriber of the target services. Equal type names, sizes and alignments, which is
    /// all iceoryx2 and this function can compare, don't establish that a subscriber's type has the same native
    /// representation as the recording publisher's (field order, byte order, enum discriminants, a type changed
    /// under the same name, another architecture or compiler); the caller vouches for that too. That holds when the journal was recorded from
    /// correct publishers of those types and has not been altered since (tickertape's checksums detect accidental
    /// damage, not a journal written with wrong contents). A subscriber of a type with invalid bit patterns, such as
    /// `bool` or an enum, has undefined behaviour in its own safe code if this doesn't hold. Replay only recordings
    /// you trust, as with upstream's own replay tool.
    ///
    /// # Errors
    ///
    /// As for [`Replayer::run`], except that unregistered services are replayed instead of refused.
    pub unsafe fn run_dynamic(self) -> Result<ReplayReport, Error> {
        self.run_with(Some(sink))
    }
}

/// Opens a publisher on `target` with the recorded type details, creating the service with the recorded settings
/// if it doesn't exist. The descriptor is validated again before anything reaches iceoryx2. Only
/// [`Replayer::run_dynamic`] uses it, whose caller promises that the sent bytes are valid values (unsafe block 7).
fn sink(
    node: &Node<ipc::Service>,
    target: &str,
    descriptor: &Descriptor,
) -> Result<Box<dyn Sink>, Error> {
    descriptor.validate().map_err(|e| Error::Unsupported {
        service: descriptor.service_name.clone(),
        reason: e.to_string(),
    })?;
    let name = service_name(target)?;
    let payload = type_detail(&descriptor.payload)?;
    let user_header = type_detail(&descriptor.user_header)?;
    let factory = open_or_create!(
        || raw_builder(node, &name, &payload, &user_header),
        target,
        Create::Replay(descriptor.settings)
    )?;
    // What the publisher will allocate, from the service's settings and types in force (as opened or created,
    // iceoryx2's defaults included), checked before it exists.
    let (strategy, initial) = match payload.variant() {
        IoxVariant::FixedSize => (AllocationStrategy::Static, 1),
        IoxVariant::Dynamic => (
            AllocationStrategy::PowerOfTwo,
            initial_slice_len(factory.static_config()),
        ),
    };
    let budget = PortBudget::check(
        target,
        factory.static_config(),
        PUBLISHER_MAX_LOANED,
        strategy != AllocationStrategy::Static,
        Some(initial),
    )?;
    let publisher = factory
        .publisher_builder()
        .max_loaned_samples(PUBLISHER_MAX_LOANED)
        .initial_max_slice_len(initial)
        .allocation_strategy(strategy)
        .create()
        .map_err(|e| Error::iox("create publisher", e))?;
    Ok(Box::new(DynamicSink {
        budget,
        target: target.to_string(),
        descriptor: descriptor.clone(),
        variant: payload.variant(),
        user_header_len: user_header.size(),
        factory,
        publisher,
    }))
}

struct DynamicSink {
    budget: PortBudget,
    target: String,
    factory: RawFactory,
    publisher: Publisher<ipc::Service, [CustomPayloadMarker], CustomHeaderMarker>,
    descriptor: Descriptor,
    variant: IoxVariant,
    user_header_len: usize,
}

impl Sink for DynamicSink {
    fn send(&mut self, user_header: &[u8], payload: &[u8]) -> Result<(), Error> {
        let elements =
            layout::check_message(user_header, payload, &self.descriptor).map_err(|e| {
                Error::Unsupported {
                    service: self.descriptor.service_name.clone(),
                    reason: e.to_string(),
                }
            })?;
        let elements = usize::try_from(elements)
            .map_err(|_| Error::Usage("element count does not fit this platform".into()))?;
        if self.variant == IoxVariant::FixedSize && elements != 1 {
            return Err(Error::Usage("a fixed-size payload has one element".into()));
        }
        // A larger slice than any before makes the publisher's data segment grow: bounded like its first one.
        self.budget.check_payload(&self.target, payload.len())?;
        // SAFETY: (unsafe block 4) `loan_custom_payload` requires: a slice length other than 1 only for a dynamic
        // payload (checked just above), and that the sample be treated as `elements * element size` bytes, which is
        // `payload.len()` (checked by `check_message`), exactly what is written below.
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
        if self.user_header_len > 0 {
            let header: *mut CustomHeaderMarker = sample.user_header_mut();
            // SAFETY: (unsafe block 6) iceoryx2 reserves `user_header_len` bytes (the validated size in the
            // service's type details, equal to `user_header.len()` by `check_message`) for the user header in the
            // loaned chunk, starting at this address; they are exclusively ours until `send` and don't overlap
            // `user_header`. As in block 3, the pointer comes from a reference to the zero-sized
            // `CustomHeaderMarker` and writing beyond it relies on the Tree Borrows aliasing model (module docs);
            // upstream's replay and publish tools do the same.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    user_header.as_ptr(),
                    header.cast::<u8>(),
                    user_header.len(),
                );
            }
        }
        // SAFETY: (unsafe block 7) Every byte of the payload and user header was written above. That those bytes
        // are valid values of the service's types is the contract of `Replayer::run_dynamic` (an `unsafe fn`), the
        // only way to reach this sink.
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
