//! The memory budget against what iceoryx2 0.10.0 really allocates (review rounds 5 and 6): ports are created
//! against a real iceoryx2 node, and for each the heap it adds (live bytes, counted by this test binary's global
//! allocator, per thread) and the shared memory it creates (the sizes of the shared-memory objects whose names carry
//! the port's id) are measured and compared with the figures of [`layout`].
//!
//! Two kinds of assertion:
//!
//! - **Totals** (`the_budget_covers_what_iceoryx2_allocates`): for small, representative shapes, and one shape per
//!   port kind with a raised setting, every estimate is at least what was measured. This shows that no term is
//!   missing, but at small shapes the totals are dominated by the fixed parts (1 MiB of heap) and by empty slots, so
//!   a per-item constant several times too small could still pass.
//! - **Slopes** (`each_per_item_constant_covers_the_measured_cost_per_item`): for each quantity the budget charges
//!   per item, the same port is measured at two values of that quantity, and the measured difference per item must
//!   be more than zero and at most the formula's cost per item, which is the production function evaluated at the
//!   same two values. Where the constant is meant to be tight, the measured cost per item must also be at least a
//!   stated fraction of it (so the measurement really exercises the term); where a constant is deliberately
//!   generous, only the upper side is asserted (see [`Bound`]). Shared-memory slopes allow for page rounding, so
//!   their two values are far enough apart that the expected growth is at least 8 pages of 16 KiB.
//!
//! The figures at the caps (128 MiB of heap, 1.5 GiB of shared memory) are the formulas evaluated at the caps, not
//! measurements.
//!
//! **Blind spot.** The counting allocator sees only the heap that goes through Rust's global allocator. iceoryx2's
//! own `HeapAllocator` calls `malloc` directly (`iceoryx2-bb-memory` `heap_allocator.rs` near line 41,
//! `iceoryx2-bb-posix` `memory.rs` near line 124); on the publish-subscribe paths (publisher, subscriber, sender,
//! receiver, connections) its only users are the subscriber's two connection lists (`port/subscriber.rs` near lines
//! 288-306: 16 bytes per `max_publishers` and 8 per expired-connection slot). Those are in
//! [`layout::subscriber_heap`] from iceoryx2's source ([`layout::SUBSCRIBER_DIRECT_HEAP_PER_PUBLISHER`],
//! [`layout::SUBSCRIBER_DIRECT_HEAP_PER_EXPIRED`]); this test doesn't measure them.
//!
//! Every port stays at a few megabytes of real allocation. Run with `--nocapture` to see the tables.

#![cfg(feature = "iceoryx2")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::fmt;
use std::sync::Mutex;
use std::time::Duration;

use iceoryx2::config::Config;
use iceoryx2::port::publisher::Publisher;
use iceoryx2::port::subscriber::Subscriber;
use iceoryx2::prelude::*;
use iceoryx2::service::dynamic_config::publish_subscribe::PublisherDetails;
use iceoryx2::service::port_factory::publish_subscribe::PortFactory as PubSubFactory;
use iceoryx2::service::static_config::message_type_details::{
    TypeDetail, TypeVariant as IoxVariant,
};
use iceoryx2::service::static_config::publish_subscribe::StaticConfig;
use iceoryx2_bb_posix::shared_memory::{AccessMode, SharedMemory, SharedMemoryBuilder};
use iceoryx2_bb_posix::system_configuration::SystemInfo;
use tickertape_iceoryx2::layout::{
    self, AdvertisedPublisher, ChunkLayout, ServiceSettings, TypeInfo, TypeVariant,
};
use tickertape_iceoryx2::recorder::{Recorder, TypedService};

// ---------------------------------------------------------------------------------------------------------------
// Heap: live bytes per thread.

thread_local! {
    static LIVE: Cell<i64> = const { Cell::new(0) };
    static PEAK: Cell<i64> = const { Cell::new(0) };
}

fn account(delta: i64) {
    // `try_with`: a thread that is being torn down still allocates; those bytes aren't measured.
    let _ = LIVE.try_with(|live| {
        let now = live.get() + delta;
        live.set(now);
        let _ = PEAK.try_with(|peak| {
            if now > peak.get() {
                peak.set(now)
            }
        });
    });
}

/// The system allocator, counting the bytes each thread has live.
struct Counting;

// SAFETY: every call is forwarded unchanged to `System`, which upholds `GlobalAlloc`'s contract; the bookkeeping
// only touches const-initialised thread-locals of `Cell<i64>` (no allocation, no destructor).
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's `layout` is passed on as it is (non-zero size, as `GlobalAlloc::alloc` requires).
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            account(layout.size() as i64);
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as for `alloc`.
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            account(layout.size() as i64);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller guarantees `ptr` came from this allocator (so from `System`) with `layout`.
        unsafe { System.dealloc(ptr, layout) };
        account(-(layout.size() as i64));
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller guarantees `ptr` came from this allocator with `layout` and that `new_size` is valid.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            account(new_size as i64 - layout.size() as i64);
        }
        p
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn live() -> i64 {
    LIVE.with(Cell::get)
}

/// Runs `f` and returns its result and the heap it left live on this thread (what it allocated and kept).
fn kept<T>(f: impl FnOnce() -> T) -> (T, u64) {
    let before = live();
    let value = f();
    (value, (live() - before).max(0) as u64)
}

/// Runs `f` and returns its result and the most heap it had live at once on this thread.
fn peak<T>(f: impl FnOnce() -> T) -> (T, u64) {
    let before = live();
    PEAK.with(|p| p.set(before));
    let value = f();
    (value, (PEAK.with(Cell::get) - before).max(0) as u64)
}

// ---------------------------------------------------------------------------------------------------------------
// Shared memory: iceoryx2's objects whose names carry a port id.

/// The shared-memory objects whose names contain `id` (iceoryx2 names a port's data segments `<id>…` and its
/// connections `<publisher id>_<subscriber id>…`), with their sizes. On Linux they are the files under `/dev/shm`;
/// on macOS iceoryx2 0.10.0 keeps a state file per object in `/tmp` (`<name>.shm_state`, holding the name of the
/// real POSIX shared-memory object, which has no path), and this is how iceoryx2 lists and opens them. An object
/// named after the port that can't be opened (and so sized) fails the test with its name and the error: skipping it
/// would leave its bytes out of the measurement.
fn shm_of(id: u128) -> Vec<(String, u64)> {
    let id = id.to_string();
    let mut found: Vec<_> = SharedMemory::list()
        .into_iter()
        .filter_map(|name| {
            let text = String::from_utf8_lossy(name.as_bytes()).into_owned();
            if !text.contains(&id) {
                return None;
            }
            let size = match SharedMemoryBuilder::new(&name).open_existing(AccessMode::Read) {
                Ok(m) => m.size() as u64,
                Err(e) => panic!(
                    "the shared-memory object {text} of port {id} can't be opened or sized: {e:?}"
                ),
            };
            Some((text, size))
        })
        .collect();
    found.sort();
    found
}

#[derive(Debug, Default, Clone, Copy)]
struct Shm {
    data: u64,
    data_objects: u64,
    connections: u64,
}

fn split(objects: &[(String, u64)]) -> Shm {
    let mut s = Shm::default();
    for (name, size) in objects {
        if name.ends_with(".data") {
            s.data += size;
            s.data_objects += 1;
        } else {
            s.connections += size;
        }
    }
    s
}

// ---------------------------------------------------------------------------------------------------------------
// The services and ports.

type Factory<P> = PubSubFactory<ipc::Service, P, ()>;

static SERIAL: Mutex<()> = Mutex::new(());

fn unique(name: &str) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    format!(
        "tt-alloc/{}-{}/{name}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

/// A node whose publishers default to `strategy`, and whose subscribers keep `expired` expired connections (the
/// global configuration's, 128, if `None`).
fn node(strategy: AllocationStrategy, expired: Option<usize>) -> Node<ipc::Service> {
    let mut config = Config::global_config().clone();
    let defaults = &mut config.defaults.publish_subscribe;
    defaults.publisher_allocation_strategy = strategy;
    if let Some(expired) = expired {
        defaults.subscriber_expired_connection_buffer = expired;
    }
    NodeBuilder::new()
        .config(&config)
        .signal_handling_mode(SignalHandlingMode::Disabled)
        .create::<ipc::Service>()
        .unwrap()
}

fn type_info(d: &TypeDetail) -> TypeInfo {
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

fn settings_of(cfg: &StaticConfig) -> ServiceSettings {
    ServiceSettings {
        history_size: cfg.history_size() as u64,
        subscriber_max_buffer_size: cfg.subscriber_max_buffer_size() as u64,
        subscriber_max_borrowed_samples: cfg.subscriber_max_borrowed_samples() as u64,
        max_publishers: cfg.max_publishers() as u64,
        max_subscribers: cfg.max_subscribers() as u64,
        max_nodes: cfg.max_nodes() as u64,
        safe_overflow: cfg.has_safe_overflow(),
    }
}

fn chunk_of(cfg: &StaticConfig) -> ChunkLayout {
    let t = cfg.message_type_details();
    layout::validate_types(
        &type_info(&t.payload),
        &type_info(&t.user_header),
        &type_info(&t.header),
    )
    .unwrap()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Fixed,
    Slice,
}

/// Ports per service in the small shapes (32 peers and the measured port, with room to spare), subscriber buffer.
const PORTS: usize = 34;
const BUFFER: usize = 1;
/// A slice publisher's first length in the shapes with many ports (small, so 32 publishers stay small).
const SMALL_SLICE: usize = 64;
/// Loaned samples, as the replayer's publishers.
const LOANED: usize = 2;

/// A service's settings (borrowed samples are always 1).
#[derive(Debug, Clone, Copy)]
struct Shape {
    max_publishers: usize,
    max_subscribers: usize,
    /// `subscriber_max_buffer_size`.
    buffer: usize,
    history: usize,
}

impl Shape {
    /// The small shape: [`PORTS`] publishers and subscribers, buffer [`BUFFER`].
    const fn small(history: usize) -> Shape {
        Shape {
            max_publishers: PORTS,
            max_subscribers: PORTS,
            buffer: BUFFER,
            history,
        }
    }

    /// The samples a publisher of this service preallocates (`max_subscribers × (buffer + 1 borrowed) + history +`
    /// [`LOANED`]).
    fn samples(&self) -> u64 {
        (self.max_subscribers * (self.buffer + 1) + self.history + LOANED) as u64
    }
}

impl fmt::Display for Shape {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "pub {} sub {} buf {} hist {}",
            self.max_publishers, self.max_subscribers, self.buffer, self.history
        )
    }
}

/// A service (`u64` or `[u8]`) opened from a node with the static default strategy and from one whose default is
/// growing (fixed-size publishers take their strategy from their node's configuration).
enum Svc {
    Fixed {
        on_static: Factory<u64>,
        on_growing: Factory<u64>,
    },
    Slice(Factory<[u8]>),
}

enum Pub {
    Fixed(Publisher<ipc::Service, u64, ()>),
    Slice(Publisher<ipc::Service, [u8], ()>),
}

enum Sub {
    Fixed(Subscriber<ipc::Service, u64, ()>),
    Slice(Subscriber<ipc::Service, [u8], ()>),
}

struct Nodes {
    on_static: Node<ipc::Service>,
    on_growing: Node<ipc::Service>,
}

impl Nodes {
    /// Nodes whose subscribers keep `expired` expired connections (the global configuration's if `None`).
    fn new(expired: Option<usize>) -> Nodes {
        Nodes {
            on_static: node(AllocationStrategy::Static, expired),
            on_growing: node(AllocationStrategy::PowerOfTwo, expired),
        }
    }
}

impl Svc {
    fn create(nodes: &Nodes, kind: Kind, name: &str, shape: Shape) -> Svc {
        let name = ServiceName::new(name).unwrap();
        match kind {
            Kind::Fixed => {
                let on_static = nodes
                    .on_static
                    .service_builder(&name)
                    .publish_subscribe::<u64>()
                    .max_publishers(shape.max_publishers)
                    .max_subscribers(shape.max_subscribers)
                    .subscriber_max_buffer_size(shape.buffer)
                    .subscriber_max_borrowed_samples(1)
                    .history_size(shape.history)
                    .create()
                    .unwrap();
                let on_growing = nodes
                    .on_growing
                    .service_builder(&name)
                    .publish_subscribe::<u64>()
                    .open()
                    .unwrap();
                Svc::Fixed {
                    on_static,
                    on_growing,
                }
            }
            Kind::Slice => Svc::Slice(
                nodes
                    .on_static
                    .service_builder(&name)
                    .publish_subscribe::<[u8]>()
                    .max_publishers(shape.max_publishers)
                    .max_subscribers(shape.max_subscribers)
                    .subscriber_max_buffer_size(shape.buffer)
                    .subscriber_max_borrowed_samples(1)
                    .history_size(shape.history)
                    .create()
                    .unwrap(),
            ),
        }
    }

    fn cfg(&self) -> &StaticConfig {
        match self {
            Svc::Fixed { on_static, .. } => on_static.static_config(),
            Svc::Slice(f) => f.static_config(),
        }
    }

    fn advertised(&self) -> Vec<AdvertisedPublisher> {
        let mut v = Vec::new();
        let mut push = |p: &PublisherDetails| {
            v.push(AdvertisedPublisher {
                samples: p.number_of_samples as u64,
                segments: u64::from(p.max_number_of_segments),
            });
            CallbackProgression::Continue
        };
        match self {
            Svc::Fixed { on_static, .. } => on_static.dynamic_config().list_publishers(&mut push),
            Svc::Slice(f) => f.dynamic_config().list_publishers(&mut push),
        }
        v
    }

    /// A publisher with a static or growing strategy; a slice publisher starts with `initial_len` elements.
    fn publisher(&self, growing: bool, initial_len: usize) -> Pub {
        match self {
            Svc::Fixed {
                on_static,
                on_growing,
            } => {
                let f = if growing { on_growing } else { on_static };
                Pub::Fixed(
                    f.publisher_builder()
                        .max_loaned_samples(LOANED)
                        .create()
                        .unwrap(),
                )
            }
            Svc::Slice(f) => Pub::Slice(
                f.publisher_builder()
                    .max_loaned_samples(LOANED)
                    .initial_max_slice_len(initial_len)
                    .allocation_strategy(if growing {
                        AllocationStrategy::PowerOfTwo
                    } else {
                        AllocationStrategy::Static
                    })
                    .create()
                    .unwrap(),
            ),
        }
    }

    fn subscriber(&self, buffer: usize) -> Sub {
        match self {
            Svc::Fixed { on_static, .. } => Sub::Fixed(
                on_static
                    .subscriber_builder()
                    .buffer_size(buffer)
                    .create()
                    .unwrap(),
            ),
            Svc::Slice(f) => {
                Sub::Slice(f.subscriber_builder().buffer_size(buffer).create().unwrap())
            }
        }
    }
}

impl Pub {
    fn id(&self) -> u128 {
        match self {
            Pub::Fixed(p) => p.id().value(),
            Pub::Slice(p) => p.id().value(),
        }
    }

    /// Sends one sample (of `len` bytes for a slice).
    fn send(&self, len: usize) {
        match self {
            Pub::Fixed(p) => {
                p.loan_uninit().unwrap().write_payload(7).send().unwrap();
            }
            Pub::Slice(p) => {
                p.loan_slice_uninit(len)
                    .unwrap()
                    .write_from_fn(|i| i as u8)
                    .send()
                    .unwrap();
            }
        }
    }
}

impl Sub {
    fn id(&self) -> u128 {
        match self {
            Sub::Fixed(s) => s.id().value(),
            Sub::Slice(s) => s.id().value(),
        }
    }

    /// Receives (and releases) every sample there is; also makes the subscriber update its connections.
    fn drain(&self) -> usize {
        let mut n = 0;
        match self {
            Sub::Fixed(s) => {
                while s.receive().unwrap().is_some() {
                    n += 1
                }
            }
            Sub::Slice(s) => {
                while s.receive().unwrap().is_some() {
                    n += 1
                }
            }
        }
        n
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Totals: each estimate against what one port measured.

const KIB: u64 = 1024;

/// One port: (measured, budget) for its heap, its data segments and the connections it created, in bytes.
struct Row {
    shape: String,
    heap: (u64, u64),
    data: (u64, u64),
    connections: (u64, u64),
}

impl Row {
    fn print(&self) {
        let f =
            |(measured, budget): (u64, u64)| format!("{:>9} / {:>9}", measured / KIB, budget / KIB);
        eprintln!(
            "| {:<88} | {} | {} | {} |",
            self.shape,
            f(self.heap),
            f(self.data),
            f(self.connections)
        );
    }

    /// Asserts that each budget is at least what was measured.
    fn assert_within(&self) {
        let ok = |(m, b): (u64, u64)| m <= b;
        assert!(
            ok(self.heap) && ok(self.data) && ok(self.connections),
            "{}: a measured figure is above the budget's (heap {:?}, data segments {:?}, connections {:?}; \
             measured, budget)",
            self.shape,
            self.heap,
            self.data,
            self.connections
        );
    }

    fn check(&self) {
        self.print();
        self.assert_within();
    }
}

fn header() {
    eprintln!(
        "\n{} {} (KiB; measured / budget)\n| {:<88} | {:^21} | {:^21} | {:^21} |\n|{}|{}|{}|{}|",
        std::env::consts::OS,
        std::env::consts::ARCH,
        "shape",
        "heap",
        "data segments",
        "connections",
        "-".repeat(90),
        "-".repeat(23),
        "-".repeat(23),
        "-".repeat(23)
    );
}

fn expired(nodes: &Nodes) -> u64 {
    nodes
        .on_static
        .config()
        .defaults
        .publish_subscribe
        .subscriber_expired_connection_buffer as u64
}

/// A subscriber with a buffer of `buffer` samples created next to `k` publishers already present: its heap
/// (creation and a first receive) and the connections it creates, against [`layout::subscriber_allocation`].
fn subscriber_with_publishers_present(
    nodes: &Nodes,
    kind: Kind,
    growing: bool,
    k: usize,
    shape: Shape,
    buffer: usize,
) -> Row {
    let svc = Svc::create(nodes, kind, &unique("sub-present"), shape);
    let pubs: Vec<_> = (0..k)
        .map(|_| svc.publisher(growing, SMALL_SLICE))
        .collect();
    let advertised = svc.advertised();
    let (sub, heap) = kept(|| {
        let sub = svc.subscriber(buffer);
        sub.drain();
        sub
    });
    let shm = split(&shm_of(sub.id()));
    let budget = layout::subscriber_allocation(
        &settings_of(svc.cfg()),
        expired(nodes),
        buffer as u64,
        &advertised,
    )
    .unwrap();
    drop((sub, pubs));
    Row {
        shape: format!(
            "subscriber, {k:>2} {} publishers present, {kind:?}, {shape}, own buf {buffer}, expired {}",
            if growing { "growing" } else { "static " },
            expired(nodes)
        ),
        heap: (heap, budget.heap),
        data: (shm.data, 0),
        connections: (shm.connections, budget.connections),
    }
}

/// A subscriber created first, then `k` growing publishers, then a receive: the heap the publishers that came
/// later cost it, against [`layout::subscriber_heap`] (the bound by `max_publishers`). The connections are created
/// by the publishers.
fn subscriber_with_later_publishers(nodes: &Nodes, kind: Kind, k: usize) -> (Row, ServiceSettings) {
    let svc = Svc::create(nodes, kind, &unique("sub-later"), Shape::small(0));
    let (sub, created) = kept(|| svc.subscriber(BUFFER));
    let pubs: Vec<_> = (0..k).map(|_| svc.publisher(true, SMALL_SLICE)).collect();
    let (_, connected) = kept(|| sub.drain());
    let settings = settings_of(svc.cfg());
    let budget = layout::subscriber_heap(&settings, expired(nodes)).unwrap();
    drop((sub, pubs));
    let row = Row {
        shape: format!("subscriber, then {k:>2} growing publishers, {kind:?}"),
        heap: (created + connected, budget),
        data: (0, 0),
        connections: (0, 0),
    };
    (row, settings)
}

/// A publisher (a slice one starting with `initial_len` elements) created next to `k` subscribers already present:
/// its heap, its data segment and the connections it creates, against [`layout::publisher_allocation`] and
/// [`layout::PortAllocation::check_data_segments`]. Also returns the size of its chunks (its first ones for a
/// slice) and the allocation [`layout::publisher_allocation`] computed for it.
fn publisher_with_subscribers_present(
    nodes: &Nodes,
    kind: Kind,
    growing: bool,
    k: usize,
    shape: Shape,
    initial_len: usize,
) -> (Row, u64, layout::PortAllocation) {
    let svc = Svc::create(nodes, kind, &unique("pub-present"), shape);
    let subs: Vec<_> = (0..k).map(|_| svc.subscriber(BUFFER)).collect();
    let (publisher, heap) = kept(|| svc.publisher(growing, initial_len));
    let shm = split(&shm_of(publisher.id()));
    let cfg = svc.cfg();
    let alloc = layout::publisher_allocation(&settings_of(cfg), LOANED as u64, growing).unwrap();
    assert_eq!(alloc.samples, shape.samples());
    let chunk = chunk_of(cfg);
    let initial = chunk
        .size_for(if kind == Kind::Slice {
            initial_len as u64
        } else {
            1
        })
        .unwrap();
    let data = alloc
        .check_data_segments(initial, initial, chunk.alignment)
        .unwrap();
    drop((publisher, subs));
    let row = Row {
        shape: format!(
            "publisher, {k:>2} subscribers present, {} {kind:?}, {shape}",
            if growing { "growing" } else { "static " }
        ),
        heap: (heap, alloc.heap),
        data: (shm.data, data),
        connections: (shm.connections, alloc.connections),
    };
    (row, initial, alloc)
}

/// A growing slice publisher whose data segment grows `steps` times while a history of 3 (and a subscriber that
/// doesn't receive yet) keep samples in the older segments: every data segment alive, against
/// [`layout::PortAllocation::check_data_segments`] for the largest sample. Then the subscriber receives everything:
/// its heap against [`layout::subscriber_heap`].
fn grown_publisher(nodes: &Nodes, steps: u32) -> (Row, Row) {
    const HISTORY: usize = 3;
    let shape = Shape {
        max_publishers: 2,
        max_subscribers: 2,
        buffer: 2,
        history: HISTORY,
    };
    let svc = Svc::create(nodes, Kind::Slice, &unique("grown"), shape);
    let (sub, sub_created) = kept(|| svc.subscriber(2));
    // As the replayer: about 4 KiB of elements first.
    let initial_len = 4096;
    let (publisher, heap) = kept(|| {
        let p = svc.publisher(true, initial_len);
        p.send(initial_len);
        // Each step needs a chunk larger than the current buckets: 1.5, 3, 6, … times the first length.
        for step in 0..steps {
            p.send(initial_len * 3 * (1 << step) / 2);
        }
        p
    });
    let largest = initial_len * 3 * (1 << (steps - 1)) / 2;
    let objects = shm_of(publisher.id());
    let shm = split(&objects);
    let cfg = svc.cfg();
    let alloc = layout::publisher_allocation(&settings_of(cfg), LOANED as u64, true).unwrap();
    let chunk = chunk_of(cfg);
    let data = alloc
        .check_data_segments(
            chunk.size_for(initial_len as u64).unwrap(),
            chunk.size_for(largest as u64).unwrap(),
            chunk.alignment,
        )
        .unwrap();
    let grown = Row {
        shape: format!(
            "publisher, growing slice, grown {steps}×, history {HISTORY} ({} data objects)",
            shm.data_objects
        ),
        heap: (heap, alloc.heap),
        data: (shm.data, data),
        connections: (shm.connections, alloc.connections),
    };
    // Older segments are kept while samples in them are referenced: more than the first segment, the management
    // object and the newest one are alive.
    assert!(shm.data_objects >= 3, "{objects:?}");
    let (received, sub_connected) = kept(|| sub.drain());
    assert!(received >= 1);
    let budget = layout::subscriber_heap(&settings_of(cfg), expired(nodes)).unwrap();
    let receiving = Row {
        shape: format!("subscriber of that publisher, after receiving {received}"),
        heap: (sub_created + sub_connected, budget),
        data: (0, 0),
        connections: (0, 0),
    };
    drop((sub, publisher));
    (grown, receiving)
}

/// The shapes with a raised setting and nothing present, one per port kind: a publisher of a service allowing
/// many subscribers ([`raised_subscribers`]), a subscriber of one allowing 200 publishers (with the default
/// expired-connection buffer, 203 is the most the recorder accepts).
fn raised_publisher(nodes: &Nodes, growing: bool) -> Row {
    let shape = Shape {
        max_subscribers: raised_subscribers(growing),
        ..Shape::small(0)
    };
    publisher_with_subscribers_present(nodes, Kind::Fixed, growing, 0, shape, SMALL_SLICE).0
}

/// The raised `max_subscribers`: 1,024 for a static publisher; 896 for a growing one, whose connections to 1,024
/// subscribers (255 used-chunk lists of 2,052 samples each, 576 KiB counted per connection) would be over the
/// 512 MiB limit.
fn raised_subscribers(growing: bool) -> usize {
    if growing { 896 } else { 1024 }
}

fn raised_subscriber(nodes: &Nodes) -> Row {
    let shape = Shape {
        max_publishers: 200,
        ..Shape::small(0)
    };
    subscriber_with_publishers_present(nodes, Kind::Fixed, false, 0, shape, BUFFER)
}

/// Every shape of review round 5: fixed-size and slice services; static and growing publishers; 0, 8 and 32 peers
/// present; history 0 and 3; publishers that connect after the subscriber; a slice publisher grown twice and three
/// times with a history. And review round 6's raised settings: a publisher allowing 1,024 subscribers (896 if
/// growing) and a subscriber allowing 200 publishers.
#[test]
fn the_budget_covers_what_iceoryx2_allocates() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let nodes = Nodes::new(None);
    header();
    for kind in [Kind::Fixed, Kind::Slice] {
        for growing in [false, true] {
            for history in [0, 3] {
                for k in [0, 8, 32] {
                    let shape = Shape::small(history);
                    subscriber_with_publishers_present(&nodes, kind, growing, k, shape, BUFFER)
                        .check();
                }
            }
        }
    }
    for kind in [Kind::Fixed, Kind::Slice] {
        for k in [8, 32] {
            subscriber_with_later_publishers(&nodes, kind, k).0.check();
        }
    }
    for kind in [Kind::Fixed, Kind::Slice] {
        for growing in [false, true] {
            for history in [0, 3] {
                for k in [0, 8, 32] {
                    let shape = Shape::small(history);
                    publisher_with_subscribers_present(
                        &nodes,
                        kind,
                        growing,
                        k,
                        shape,
                        SMALL_SLICE,
                    )
                    .0
                    .check();
                }
            }
        }
    }
    for steps in [2, 3] {
        let (grown, receiving) = grown_publisher(&nodes, steps);
        grown.check();
        receiving.check();
    }
    for growing in [false, true] {
        raised_publisher(&nodes, growing).check();
    }
    raised_subscriber(&nodes).check();
}

// ---------------------------------------------------------------------------------------------------------------
// Slopes: each per-item constant against the measured cost per item.

/// How a slope is asserted besides `measured per item ≤ constant`.
#[derive(Debug, Clone, Copy)]
enum Bound {
    /// The constant is meant to be tight: the measured cost per item must be at least this fraction of it, so the
    /// shapes really exercise the term (a lower figure means the term no longer costs what the formula says, or the
    /// measurement misses it).
    AtLeast(f64),
    /// The constant is deliberately generous (the text where it is used says by how much): the upper side only.
    Generous,
    /// The constant is deliberately generous, and the measured cost per item must be at least this many bytes: a
    /// floor iceoryx2's source gives for what each item holds whatever the rounding (the text where it is used says
    /// which).
    AtLeastBytes(f64),
}

/// For constants iceoryx2's source gives exactly (8-byte reference counters, a data segment's chunk and its 4-byte
/// index entry, a connection's 8-byte queue entries and 1-byte used-chunk entries): the measured cost per item must
/// come within 10% of the constant.
const EXACT: Bound = Bound::AtLeast(0.9);
/// For constants measured and then rounded up with a margin of less than 2× (a publisher's subscriber slot and
/// history entry, a subscriber's connection to a growing publisher): at least half. Below half, the constant would
/// be more than twice what iceoryx2 allocates, which is not what these constants claim to be.
const MEASURED: Bound = Bound::AtLeast(0.5);

/// One quantity the budget charges per item, measured at `items.0` and `items.1`.
struct Slope {
    what: String,
    items: (u64, u64),
    /// The measured bytes at each.
    measured: (u64, u64),
    /// The formula's bytes per item: the production function (`layout`) evaluated at the two sizes, its difference
    /// divided by the items between them (less the parts named where a slope doesn't exercise them).
    constant: f64,
    /// The bytes by which the measured difference may exceed `constant × items` (and fall short of the lower
    /// bound): 0 for the heap; for shared memory, one page of the OS (the objects' sizes may be rounded up to whole
    /// pages) and one [`layout::COUNTED_PAGE_BYTES`] page (the formula rounds each object up to those), see
    /// [`shm_allowance`].
    allowance: u64,
    bound: Bound,
}

/// The growth a shared-memory slope must expect at least: 8 pages of 16 KiB (macOS arm64's page), so that a term
/// that isn't measured at all (zero growth) can't pass within the allowance.
const MIN_SHM_GROWTH: u64 = 8 * 16 * KIB;

/// The allowance of a shared-memory slope: the OS page `page` and the formula's [`layout::COUNTED_PAGE_BYTES`].
fn shm_allowance(page: u64) -> u64 {
    page + layout::COUNTED_PAGE_BYTES
}

impl Slope {
    fn check(&self) {
        let n = (self.items.1 - self.items.0) as f64;
        let diff = self.measured.1 as f64 - self.measured.0 as f64;
        let per = diff / n;
        let expected = self.constant * n;
        let lower = match self.bound {
            Bound::AtLeast(f) => format!("≥ {f:.1} × constant"),
            Bound::Generous => "upper only".to_string(),
            Bound::AtLeastBytes(b) => format!("≥ {b:.0} bytes"),
        };
        eprintln!(
            "| {:<80} | {:>6} → {:<6} | {:>10.1} | {:>10.1} | {:>5.2} | {:<18} |",
            self.what,
            self.items.0,
            self.items.1,
            per,
            self.constant,
            per / self.constant,
            lower
        );
        let allowance = self.allowance as f64;
        if self.allowance > 0 {
            // The sizes must be far enough apart that the allowance can't hide the term.
            assert!(
                expected >= MIN_SHM_GROWTH as f64,
                "{}: the formula expects {expected:.0} bytes of growth between {:?} items, less than {MIN_SHM_GROWTH}: \
                 choose sizes further apart",
                self.what,
                self.items
            );
            if let Bound::AtLeast(f) = self.bound {
                assert!(
                    f * expected > allowance,
                    "{}: the lower bound ({f} × {expected:.0} bytes) is within the allowance ({allowance} bytes)",
                    self.what
                );
            }
        }
        assert!(
            diff > 0.0,
            "{}: no growth measured ({:?} bytes at {:?} items); the formula expects {expected:.0} bytes",
            self.what,
            self.measured,
            self.items
        );
        assert!(
            diff <= expected + allowance,
            "{}: {per:.1} bytes per item measured ({:?} bytes at {:?} items), more than the formula's {:.1}",
            self.what,
            self.measured,
            self.items,
            self.constant
        );
        if let Bound::AtLeast(f) = self.bound {
            assert!(
                diff >= f * expected - allowance,
                "{}: {per:.1} bytes per item measured ({:?} bytes at {:?} items), less than {f} of the formula's \
                 {:.1}: the constant is no longer tight, or the shapes don't exercise it",
                self.what,
                self.measured,
                self.items,
                self.constant
            );
        }
        if let Bound::AtLeastBytes(b) = self.bound {
            assert!(
                diff >= b * n - allowance,
                "{}: {per:.1} bytes per item measured ({:?} bytes at {:?} items), less than the {b:.0} each item \
                 holds at least",
                self.what,
                self.measured,
                self.items
            );
        }
    }
}

fn slope_header() {
    eprintln!(
        "\n{} {} (bytes per item)\n| {:<80} | {:^15} | {:^10} | {:^10} | {:^5} | {:<18} |\n|{}|{}|{}|{}|{}|{}|",
        std::env::consts::OS,
        std::env::consts::ARCH,
        "quantity",
        "items",
        "measured",
        "formula",
        "ratio",
        "lower bound",
        "-".repeat(82),
        "-".repeat(17),
        "-".repeat(12),
        "-".repeat(12),
        "-".repeat(7),
        "-".repeat(20)
    );
}

/// The formula's bytes per item between two values of a quantity: `budget` (a `layout` function) at each, their
/// difference divided by the items between them.
fn per_item(items: (u64, u64), budget: (u64, u64)) -> f64 {
    (budget.1 as f64 - budget.0 as f64) / (items.1 - items.0) as f64
}

/// [`layout::subscriber_heap`] of `settings` (with an expired-connection buffer of `expired`) for the state in
/// which `connected` of its connection slots hold a growing publisher's segment table. The budget doesn't depend on
/// how many publishers are connected: it charges every slot as if it held one (it bounds later publishers, which
/// the subscriber can't decline). So the state's figure is the budget less the table charge
/// ([`layout::SUBSCRIBER_HEAP_PER_GROWING_PUBLISHER`]) of each slot that doesn't hold one. The slots themselves (and
/// the `malloc`'d list entry of each) are in the budget whatever the state, as they are allocated with the
/// subscriber, so the difference between two states leaves out their cost, as the measured difference does.
fn subscriber_heap_connected(settings: &ServiceSettings, expired: u64, connected: u64) -> u64 {
    let slots = expired.max(settings.subscriber_max_borrowed_samples) + settings.max_publishers;
    assert!(connected <= settings.max_publishers);
    layout::subscriber_heap(settings, expired).unwrap()
        - (slots - connected) * layout::SUBSCRIBER_HEAP_PER_GROWING_PUBLISHER
}

/// For each quantity the budget charges per item, the measured cost per item at two values against the formula's
/// cost per item at the same two values (the production function evaluated there): at most that, at least the
/// stated fraction of it where the constant is meant to be tight ([`Bound`]), and more than nothing. Shared-memory
/// slopes, whose allowance includes pages, use sizes at least [`MIN_SHM_GROWTH`] apart in expected bytes.
#[test]
fn each_per_item_constant_covers_the_measured_cost_per_item() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let nodes = Nodes::new(None);
    let page = SystemInfo::PageSize.value() as u64;
    let shm = shm_allowance(page);
    slope_header();

    // Subscriber heap per growing publisher that connects after it (8 and 32, one service shape): the view of its
    // data segment and the segment table it allocates. Both subscribers allocate their connection slots (and the
    // `malloc`'d list entries) up front, so their cost cancels from the measured difference; the formula's figure is
    // `subscriber_heap` for the two states measured, 8 and 32 of the slots holding a growing publisher's table
    // (`subscriber_heap_connected`), whose difference leaves the slot cost out too.
    let expired_default = expired(&nodes);
    for kind in [Kind::Fixed, Kind::Slice] {
        let [(a, settings), (b, settings_b)] =
            [8, 32].map(|k| subscriber_with_later_publishers(&nodes, kind, k));
        assert_eq!(
            settings, settings_b,
            "the two states share one service shape"
        );
        a.assert_within();
        b.assert_within();
        let items = (8, 32);
        Slope {
            what: format!("subscriber heap per growing publisher connected later, {kind:?}"),
            items,
            measured: (a.heap.0, b.heap.0),
            constant: per_item(
                items,
                (
                    subscriber_heap_connected(&settings, expired_default, items.0),
                    subscriber_heap_connected(&settings, expired_default, items.1),
                ),
            ),
            allowance: 0,
            bound: MEASURED,
        }
        .check();
    }

    // Subscriber heap per connection slot, nothing present: max_publishers 34 and 200 (the raised shape), and an
    // expired-connection buffer of 8 and 128. The formula's charge per slot (`subscriber_allocation` at the two
    // values) less what no publisher exercises here (a growing publisher's segment table) and what this test can't
    // see (the direct-`malloc` list entries) is SUBSCRIBER_HEAP_PER_CONNECTION, 8 KiB, about 2.2× the measured
    // 3.7 KB: generous.
    let not_exercised = layout::SUBSCRIBER_HEAP_PER_GROWING_PUBLISHER as f64;
    for kind in [Kind::Fixed, Kind::Slice] {
        let [a, b] = [34, 200].map(|max_publishers| {
            let shape = Shape {
                max_publishers,
                ..Shape::small(0)
            };
            subscriber_with_publishers_present(&nodes, kind, false, 0, shape, BUFFER)
        });
        a.assert_within();
        b.assert_within();
        let items = (34, 200);
        Slope {
            what: format!(
                "subscriber heap per connection slot (max_publishers), nothing present, {kind:?}"
            ),
            items,
            measured: (a.heap.0, b.heap.0),
            constant: per_item(items, (a.heap.1, b.heap.1))
                - not_exercised
                - layout::SUBSCRIBER_DIRECT_HEAP_PER_PUBLISHER as f64,
            allowance: 0,
            bound: Bound::Generous,
        }
        .check();
    }
    let [a, b] = [8, 128].map(|expired| {
        let nodes = Nodes::new(Some(expired));
        subscriber_with_publishers_present(&nodes, Kind::Fixed, false, 0, Shape::small(0), BUFFER)
    });
    a.assert_within();
    b.assert_within();
    let items = (8, 128);
    Slope {
        what: "subscriber heap per expired-connection slot, nothing present".to_string(),
        items,
        measured: (a.heap.0, b.heap.0),
        constant: per_item(items, (a.heap.1, b.heap.1))
            - not_exercised
            - layout::SUBSCRIBER_DIRECT_HEAP_PER_EXPIRED as f64,
        allowance: 0,
        bound: Bound::Generous,
    }
    .check();

    // Publisher heap per subscriber slot, no subscriber: max_subscribers 34 and the raised value (1,024 static,
    // 896 growing; the raised shapes). Each slot also adds `buffer + borrowed` samples, so the formula's slope is
    // PUBLISHER_HEAP_PER_SUBSCRIBER plus their reference counters (2 × 8 bytes per data segment).
    for growing in [false, true] {
        let raised = raised_subscribers(growing);
        let [a, b] = [34, raised].map(|max_subscribers| {
            let shape = Shape {
                max_subscribers,
                ..Shape::small(0)
            };
            publisher_with_subscribers_present(&nodes, Kind::Fixed, growing, 0, shape, SMALL_SLICE)
                .0
        });
        a.assert_within();
        b.assert_within();
        let items = (34, raised as u64);
        Slope {
            what: format!(
                "publisher heap per subscriber slot (max_subscribers), {}",
                if growing { "growing" } else { "static" }
            ),
            items,
            measured: (a.heap.0, b.heap.0),
            constant: per_item(items, (a.heap.1, b.heap.1)),
            allowance: 0,
            bound: MEASURED,
        }
        .check();
    }

    // Publisher heap per sample of the subscribers' buffers: subscriber_max_buffer_size 1 and 31 with 34
    // subscribers allowed, 1,020 more samples; 8 bytes of reference counter per data segment each.
    for growing in [false, true] {
        let shapes = [1, 31].map(|buffer| Shape {
            buffer,
            ..Shape::small(0)
        });
        let [a, b] = shapes.map(|shape| {
            publisher_with_subscribers_present(&nodes, Kind::Fixed, growing, 0, shape, SMALL_SLICE)
                .0
        });
        a.assert_within();
        b.assert_within();
        let items = (shapes[0].samples(), shapes[1].samples());
        Slope {
            what: format!(
                "publisher heap per buffer sample (subscriber_max_buffer_size), {}",
                if growing { "growing" } else { "static" }
            ),
            items,
            measured: (a.heap.0, b.heap.0),
            constant: per_item(items, (a.heap.1, b.heap.1)),
            allowance: 0,
            bound: EXACT,
        }
        .check();
    }

    // Per history sample, with one subscriber present: the publisher's heap (PUBLISHER_HEAP_PER_HISTORY_ENTRY and
    // 8 bytes of reference counter per data segment), its data segment (a chunk and an index entry per sample) and,
    // for the growing publisher, its connection (a used-chunk entry per sample for each of its 255 data segments):
    // `publisher_allocation`, `check_data_segments` and `connection_bytes` at the two histories. A static
    // fixed-size publisher, history 0 and 16,384 (about 1 MB of chunks); a growing slice publisher with chunks of
    // about 1 KiB, history 0 and 4,096 (about 4 MB of chunks, 1 MB of used-chunk lists). A static publisher's
    // connection grows by one byte per sample, which 65,536 history samples (the cap) would take only to 4 pages of
    // 16 KiB; it is measured by the buffer below.
    for (kind, growing, initial_len, history) in [
        (Kind::Fixed, false, SMALL_SLICE, 16_384),
        (Kind::Slice, true, 1024, 4096),
    ] {
        let shapes = [0, history].map(|history| Shape {
            max_publishers: 2,
            max_subscribers: 2,
            buffer: 1,
            history,
        });
        let [(a, bucket, alloc_a), (b, _, alloc_b)] = shapes.map(|shape| {
            publisher_with_subscribers_present(&nodes, kind, growing, 1, shape, initial_len)
        });
        a.assert_within();
        b.assert_within();
        let items = (0, history as u64);
        let label = if growing {
            "growing slice"
        } else {
            "static fixed-size"
        };
        Slope {
            what: format!("publisher heap per history sample, {label}"),
            items,
            measured: (a.heap.0, b.heap.0),
            constant: per_item(items, (a.heap.1, b.heap.1)),
            allowance: 0,
            bound: MEASURED,
        }
        .check();
        Slope {
            what: format!("data segment per sample ({bucket}-byte chunk, index entry), {label}"),
            items,
            measured: (a.data.0, b.data.0),
            constant: per_item(items, (a.data.1, b.data.1)),
            allowance: shm,
            bound: EXACT,
        }
        .check();
        if growing {
            Slope {
                what: format!(
                    "connection per publisher sample ({} used-chunk bytes), {label}",
                    alloc_a.segments
                ),
                items,
                measured: (a.connections.0, b.connections.0),
                constant: per_item(
                    items,
                    (
                        one_connection(&alloc_a, BUFFER),
                        one_connection(&alloc_b, BUFFER),
                    ),
                ),
                allowance: shm,
                bound: EXACT,
            }
            .check();
        }
    }

    // Connection per publisher sample, static fixed-size publisher (one used-chunk byte per sample): 8 subscribers
    // allowed with buffers of 1 and 32,768, so 262,136 more samples (about 15 MB of chunks), one subscriber present.
    let shapes = [1, 32_768].map(|buffer| Shape {
        max_publishers: 2,
        max_subscribers: 8,
        buffer,
        history: 0,
    });
    let [(a, _, alloc_a), (b, _, alloc_b)] = shapes.map(|shape| {
        publisher_with_subscribers_present(&nodes, Kind::Fixed, false, 1, shape, SMALL_SLICE)
    });
    a.assert_within();
    b.assert_within();
    let items = (alloc_a.samples, alloc_b.samples);
    Slope {
        what: format!(
            "connection per publisher sample ({} used-chunk byte), static fixed-size",
            alloc_a.segments
        ),
        items,
        measured: (a.connections.0, b.connections.0),
        constant: per_item(
            items,
            (
                one_connection(&alloc_a, BUFFER),
                one_connection(&alloc_b, BUFFER),
            ),
        ),
        allowance: shm,
        bound: EXACT,
    }
    .check();

    // Connection shared memory per connection: a publisher with 8 and 32 subscribers present, one service shape
    // (34 subscribers allowed). The formula charges by capacity, not by connected subscribers: the publisher's budget
    // is `max_subscribers × connection_bytes`, each connection sized by the publisher's samples and segments, which
    // are the same in both states. So the formula's figure for k connected subscribers is k × `connection_bytes`
    // (`one_connection`) at those samples and segments, and the expected difference is that per connection: the
    // capacity-based charge per subscriber, as the assertion below checks. Each is counted in whole 64 KiB pages
    // (the largest page size of the supported targets), while a small connection takes a few KiB (one or two
    // 16 KiB pages on macOS): generous by 2× to 20× at these shapes. The lower side is what each connection holds
    // whatever the rounding, from iceoryx2's source: its used-chunk lists (one byte per sample per segment) and its
    // two queues of 8-byte entries (`connection_bytes` without the fixed parts, which are counted with a margin,
    // or the rounding).
    for (kind, growing) in [(Kind::Fixed, false), (Kind::Slice, true)] {
        let [(a, _, alloc), (b, _, alloc_b)] = [8, 32].map(|k| {
            publisher_with_subscribers_present(
                &nodes,
                kind,
                growing,
                k,
                Shape::small(0),
                SMALL_SLICE,
            )
        });
        assert_eq!(alloc, alloc_b, "the two states share one budget");
        let items = (8, 32);
        let one = one_connection(&alloc, BUFFER);
        assert_eq!(
            alloc.connections,
            PORTS as u64 * one,
            "the budget charges a connection per subscriber allowed"
        );
        let held = alloc.segments * alloc.samples
            + (8 * (BUFFER as u64 + 1) + 7)
            + (8 * (BUFFER as u64 + 2) + 7);
        Slope {
            what: format!(
                "connections per connection (subscribers present), {} {kind:?}",
                if growing { "growing" } else { "static" }
            ),
            items,
            measured: (a.connections.0, b.connections.0),
            constant: per_item(items, (items.0 * one, items.1 * one)),
            allowance: 0,
            bound: Bound::AtLeastBytes(held as f64),
        }
        .check();
    }

    // Connection shared memory per buffer entry: a subscriber with a buffer of 1 and of 32,768 samples created next
    // to a publisher (it creates the connection): an entry in the submission queue and one in the completion queue,
    // `subscriber_allocation` at the two buffers.
    let shape = Shape {
        max_publishers: 2,
        max_subscribers: 1,
        buffer: 32_768,
        history: 0,
    };
    let [a, b] = [1, 32_768].map(|buffer| {
        subscriber_with_publishers_present(&nodes, Kind::Fixed, false, 1, shape, buffer)
    });
    a.assert_within();
    b.assert_within();
    let items = (1, 32_768);
    Slope {
        what: "connection per subscriber buffer entry (two queue entries)".to_string(),
        items,
        measured: (a.connections.0, b.connections.0),
        constant: per_item(items, (a.connections.1, b.connections.1)),
        allowance: shm,
        bound: EXACT,
    }
    .check();
}

/// [`layout::connection_bytes`] of one connection of the publisher `alloc` to a subscriber with a buffer of `buffer`
/// and 1 borrowed sample (the small shapes' subscribers).
fn one_connection(alloc: &layout::PortAllocation, buffer: usize) -> u64 {
    layout::connection_bytes(alloc.samples, alloc.segments, buffer as u64, 1).unwrap()
}

/// Review round 5, item 3: the recorder checks a service's settings before anything that iceoryx2 sizes by them.
/// Listing the publishers present allocates two vectors of `max_publishers` entries; a service whose
/// `max_publishers` is over the cap (here 40,000) must be refused without them, so recording it peaks at the heap
/// of recording a service refused for a small setting.
#[test]
fn the_recorder_validates_settings_before_listing_publishers() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let node = node(AllocationStrategy::Static, None);
    let record = |name: &str| {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::builder(dir.path().join("j"))
            .typed(TypedService::<u64, ()>::new(name))
            .handle_signals(false)
            .max_duration(Duration::from_millis(300))
            .build()
            .unwrap();
        let (report, peak) = peak(|| recorder.run().unwrap());
        (
            report.services[0].last_error.clone().unwrap_or_default(),
            peak,
        )
    };
    // A setting over its cap that the old order checked after listing too, with 2 publishers to list.
    let small = unique("small-publishers");
    let _small = node
        .service_builder(&ServiceName::new(&small).unwrap())
        .publish_subscribe::<u64>()
        .history_size(70_000)
        .create()
        .unwrap();
    let (small_error, small_peak) = record(&small);
    assert!(
        small_error.contains("history_size is 70000"),
        "{small_error}"
    );

    const PUBLISHERS: usize = 40_000;
    let many = unique("many-publishers");
    let _many = node
        .service_builder(&ServiceName::new(&many).unwrap())
        .publish_subscribe::<u64>()
        .max_publishers(PUBLISHERS)
        .create()
        .unwrap();
    let (many_error, many_peak) = record(&many);
    assert!(
        many_error.contains("max_publishers is 40000"),
        "{many_error}"
    );
    let list = (PUBLISHERS * (size_of::<PublisherDetails>() + 8)) as u64;
    eprintln!(
        "recorder peak heap: {small_peak} bytes (history over its cap), {many_peak} bytes (max_publishers \
         {PUBLISHERS}); listing them would allocate {list} bytes"
    );
    assert!(
        list > 4 << 20,
        "the test needs a list that stands out: {list}"
    );
    assert!(
        many_peak < small_peak + list / 4,
        "the recorder allocated in proportion to max_publishers before refusing the service: {many_peak} bytes, \
         {small_peak} bytes for a small one, {list} bytes to list"
    );
}
