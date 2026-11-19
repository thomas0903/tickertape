//! The memory budget against what iceoryx2 0.10.0 really allocates (review round 5): ports of small, representative
//! shapes are created against a real iceoryx2 node, and for each shape the heap they add (live bytes, counted by
//! this test binary's global allocator, per thread) and the shared memory they create (the sizes of the
//! shared-memory objects whose names carry the port's id) are measured and compared with the figures of
//! [`layout`]. A formula that comes out below a measured figure is wrong.
//!
//! Every shape stays at a few megabytes. Run with `--nocapture` to see the table.

#![cfg(feature = "iceoryx2")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
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
/// real POSIX shared-memory object, which has no path), and this is how iceoryx2 lists and opens them.
fn shm_of(id: u128) -> Vec<(String, u64)> {
    let id = id.to_string();
    let mut found: Vec<_> = SharedMemory::list()
        .into_iter()
        .filter_map(|name| {
            let text = String::from_utf8_lossy(name.as_bytes()).into_owned();
            if !text.contains(&id) {
                return None;
            }
            let size = SharedMemoryBuilder::new(&name)
                .open_existing(AccessMode::Read)
                .map(|m| m.size() as u64)
                .ok()?;
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

fn node(strategy: AllocationStrategy) -> Node<ipc::Service> {
    let mut config = Config::global_config().clone();
    config
        .defaults
        .publish_subscribe
        .publisher_allocation_strategy = strategy;
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

/// Loaned samples, as the replayer's publishers.
const LOANED: usize = 2;

struct Nodes {
    on_static: Node<ipc::Service>,
    on_growing: Node<ipc::Service>,
}

impl Svc {
    fn create(
        nodes: &Nodes,
        kind: Kind,
        name: &str,
        ports: usize,
        buffer: usize,
        history: usize,
    ) -> Svc {
        let name = ServiceName::new(name).unwrap();
        match kind {
            Kind::Fixed => {
                let on_static = nodes
                    .on_static
                    .service_builder(&name)
                    .publish_subscribe::<u64>()
                    .max_publishers(ports)
                    .max_subscribers(ports)
                    .subscriber_max_buffer_size(buffer)
                    .subscriber_max_borrowed_samples(1)
                    .history_size(history)
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
                    .max_publishers(ports)
                    .max_subscribers(ports)
                    .subscriber_max_buffer_size(buffer)
                    .subscriber_max_borrowed_samples(1)
                    .history_size(history)
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
// The table.

const KIB: u64 = 1024;

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
            "| {:<64} | {} | {} | {} |",
            self.shape,
            f(self.heap),
            f(self.data),
            f(self.connections)
        );
    }

    fn check(&self) {
        self.print();
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
}

fn header() {
    eprintln!(
        "\n{} {} (KiB; measured / budget)\n| {:<64} | {:^21} | {:^21} | {:^21} |\n|{}|{}|{}|{}|",
        std::env::consts::OS,
        std::env::consts::ARCH,
        "shape",
        "heap",
        "data segments",
        "connections",
        "-".repeat(66),
        "-".repeat(23),
        "-".repeat(23),
        "-".repeat(23)
    );
}

/// Ports per service (32 peers and the measured port, with room to spare), subscriber buffer.
const PORTS: usize = 34;
const BUFFER: usize = 1;
/// A slice publisher's first length in the shapes with many ports (small, so 32 publishers stay small).
const SMALL_SLICE: usize = 64;

fn expired(nodes: &Nodes) -> u64 {
    nodes
        .on_static
        .config()
        .defaults
        .publish_subscribe
        .subscriber_expired_connection_buffer as u64
}

/// A subscriber created next to `k` publishers already present: its heap (creation and a first receive) and the
/// connections it creates, against [`layout::subscriber_allocation`].
fn subscriber_with_publishers_present(
    nodes: &Nodes,
    kind: Kind,
    growing: bool,
    k: usize,
    history: usize,
) -> Row {
    let svc = Svc::create(nodes, kind, &unique("sub-present"), PORTS, BUFFER, history);
    let pubs: Vec<_> = (0..k)
        .map(|_| svc.publisher(growing, SMALL_SLICE))
        .collect();
    let advertised = svc.advertised();
    let (sub, heap) = kept(|| {
        let sub = svc.subscriber(BUFFER);
        sub.drain();
        sub
    });
    let shm = split(&shm_of(sub.id()));
    let budget = layout::subscriber_allocation(
        &settings_of(svc.cfg()),
        expired(nodes),
        BUFFER as u64,
        &advertised,
    )
    .unwrap();
    drop((sub, pubs));
    Row {
        shape: format!(
            "subscriber, {k:>2} {} publishers present, {kind:?}, history {history}",
            if growing { "growing" } else { "static " }
        ),
        heap: (heap, budget.heap),
        data: (shm.data, 0),
        connections: (shm.connections, budget.connections),
    }
}

/// A subscriber created first, then `k` growing publishers, then a receive: the heap the publishers that came
/// later cost it, against [`layout::subscriber_heap`] (the bound by `max_publishers`). The connections are created
/// by the publishers.
fn subscriber_with_later_publishers(nodes: &Nodes, kind: Kind, k: usize) -> Row {
    let svc = Svc::create(nodes, kind, &unique("sub-later"), PORTS, BUFFER, 0);
    let (sub, created) = kept(|| svc.subscriber(BUFFER));
    let pubs: Vec<_> = (0..k).map(|_| svc.publisher(true, SMALL_SLICE)).collect();
    let (_, connected) = kept(|| sub.drain());
    let budget = layout::subscriber_heap(&settings_of(svc.cfg()), expired(nodes)).unwrap();
    drop((sub, pubs));
    Row {
        shape: format!("subscriber, then {k:>2} growing publishers, {kind:?}"),
        heap: (created + connected, budget),
        data: (0, 0),
        connections: (0, 0),
    }
}

/// A publisher created next to `k` subscribers already present: its heap, its data segment and the connections
/// it creates, against [`layout::publisher_allocation`] and [`layout::PortAllocation::check_data_segments`].
fn publisher_with_subscribers_present(
    nodes: &Nodes,
    kind: Kind,
    growing: bool,
    k: usize,
    history: usize,
) -> Row {
    let svc = Svc::create(nodes, kind, &unique("pub-present"), PORTS, BUFFER, history);
    let subs: Vec<_> = (0..k).map(|_| svc.subscriber(BUFFER)).collect();
    let (publisher, heap) = kept(|| svc.publisher(growing, SMALL_SLICE));
    let shm = split(&shm_of(publisher.id()));
    let cfg = svc.cfg();
    let alloc = layout::publisher_allocation(&settings_of(cfg), LOANED as u64, growing).unwrap();
    let chunk = chunk_of(cfg);
    let initial = chunk
        .size_for(if kind == Kind::Slice {
            SMALL_SLICE as u64
        } else {
            1
        })
        .unwrap();
    let data = alloc
        .check_data_segments(initial, initial, chunk.alignment)
        .unwrap();
    drop((publisher, subs));
    Row {
        shape: format!(
            "publisher, {k:>2} subscribers present, {} {kind:?}, history {history}",
            if growing { "growing" } else { "static " }
        ),
        heap: (heap, alloc.heap),
        data: (shm.data, data),
        connections: (shm.connections, alloc.connections),
    }
}

/// A growing slice publisher whose data segment grows `steps` times while a history of 3 (and a subscriber that
/// doesn't receive yet) keep samples in the older segments: every data segment alive, against
/// [`layout::PortAllocation::check_data_segments`] for the largest sample. Then the subscriber receives everything:
/// its heap against [`layout::subscriber_heap`].
fn grown_publisher(nodes: &Nodes, steps: u32) -> (Row, Row) {
    const HISTORY: usize = 3;
    let svc = Svc::create(nodes, Kind::Slice, &unique("grown"), 2, 2, HISTORY);
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

/// Every shape of review round 5: fixed-size and slice services; static and growing publishers; 0, 8 and 32 peers
/// present; history 0 and 3; publishers that connect after the subscriber; a slice publisher grown twice and three
/// times with a history.
#[test]
fn the_budget_covers_what_iceoryx2_allocates() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let nodes = Nodes {
        on_static: node(AllocationStrategy::Static),
        on_growing: node(AllocationStrategy::PowerOfTwo),
    };
    header();
    for kind in [Kind::Fixed, Kind::Slice] {
        for growing in [false, true] {
            for history in [0, 3] {
                for k in [0, 8, 32] {
                    subscriber_with_publishers_present(&nodes, kind, growing, k, history).check();
                }
            }
        }
    }
    for kind in [Kind::Fixed, Kind::Slice] {
        for k in [8, 32] {
            subscriber_with_later_publishers(&nodes, kind, k).check();
        }
    }
    for kind in [Kind::Fixed, Kind::Slice] {
        for growing in [false, true] {
            for history in [0, 3] {
                for k in [0, 8, 32] {
                    publisher_with_subscribers_present(&nodes, kind, growing, k, history).check();
                }
            }
        }
    }
    for steps in [2, 3] {
        let (grown, receiving) = grown_publisher(&nodes, steps);
        grown.check();
        receiving.check();
    }
}

/// Review round 5, item 3: the recorder checks a service's settings before anything that iceoryx2 sizes by them.
/// Listing the publishers present allocates two vectors of `max_publishers` entries; a service whose
/// `max_publishers` is over the cap (here 40,000) must be refused without them, so recording it peaks at the heap
/// of recording a service refused for a small setting.
#[test]
fn the_recorder_validates_settings_before_listing_publishers() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let node = node(AllocationStrategy::Static);
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
