package compare;

import com.fasterxml.jackson.databind.ObjectMapper;
import org.HdrHistogram.Histogram;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.file.*;
import java.time.Instant;
import java.util.*;

/** Shared roles and schedule. Receive implementations call Observer after validating
 * framing; no payload copy is required by the common measurement interface. */
public final class Harness {
    public interface Observer { void record(long seq, long intended, int payload); }
    public interface Endpoint extends AutoCloseable {
        default void send(byte[] bytes) throws Exception { throw new UnsupportedOperationException(); }
        default boolean poll(Observer observer) throws Exception { throw new UnsupportedOperationException(); }
        default void sync() throws Exception { }
        default String syncOperation() { return null; }
        /** The closest equivalent of a periodic checkpoint (sustained durability workload); returns its name. */
        default String periodicSync() throws Exception {
            throw new UnsupportedOperationException("periodic durability is unsupported for this tool");
        }
        default void connected() throws Exception { }
        /** Called on every iteration of the paced writer's busy-wait, between records. */
        default void idle() { }
        /** True if a writer endpoint makes no progress without a subscriber, also in the warm-up. */
        default boolean needsPeer() { return false; }
        /** The effective tool settings of this endpoint, recorded in the result. */
        default Map<String,Object> settings() { return Map.of(); }
        @Override void close() throws Exception;
    }
    public interface Factory { Endpoint open(String role, Path dir) throws Exception; }
    static final ObjectMapper JSON = new ObjectMapper();
    static final long TIMEOUT = 120_000_000_000L;
    /** Set before endpoints open when the run requests periodic durability (--sync-every-ms). */
    public static volatile boolean periodicSyncRequested = false;
    /** The run's options, set before endpoints open. */
    public static volatile Map<String,String> options = Map.of();
    public enum Phase { MEASURED, FULL_WARMUP, FRESH_WARMUP }
    /** What endpoints are opened for now; the long warm-up opens scratch endpoints of its own. */
    public static volatile Phase phase = Phase.MEASURED;
    public static long now() {
        Instant t = Instant.now();
        return t.getEpochSecond() * 1_000_000_000L + t.getNano();
    }
    static long barrier(Path control, String role) throws Exception {
        Files.write(control.resolve(role + ".ready"), new byte[0]);
        long deadline = System.nanoTime() + TIMEOUT;
        while (!Files.exists(control.resolve("start"))) {
            if (System.nanoTime() > deadline) throw new IllegalStateException("barrier timed out");
            Thread.sleep(1);
        }
        return Long.parseLong(Files.readString(control.resolve("start")));
    }
    static Map<String,Object> emptyHistogram() {
        Map<String,Object> r = new LinkedHashMap<>();
        r.put("count", 0L);
        for (String k : new String[]{"min_us","p50_us","p90_us","p99_us","p99_9_us","p99_99_us","max_us"}) r.put(k, null);
        return r;
    }
    public static Map<String,Object> histogram(Histogram h) {
        Map<String,Object> r = new LinkedHashMap<>();
        r.put("count", h.getTotalCount());
        r.put("min_us", h.getMinValue()/1000.0);
        double[] qs = {50,90,99,99.9,99.99};
        String[] names = {"p50_us","p90_us","p99_us","p99_9_us","p99_99_us"};
        for (int i=0;i<qs.length;i++) r.put(names[i],h.getValueAtPercentile(qs[i])/1000.0);
        r.put("max_us", h.getMaxValue()/1000.0);
        return r;
    }
    /** Records a batch of the send or poll loop: small enough that the loops are compiled as ordinary methods in
     * the warm-up, not only on-stack-replaced inside one long call. */
    static final int BATCH = 256;
    static final long LATE_NS = 1_000;
    /** Receives slower than this are logged as episodes (they are in the histogram either way). */
    static final long SLOW_NS = 100_000;

    /** Runs of consecutive late sends or slow receives, for attributing tail samples after the run. Kept in
     * preallocated arrays; the hot path pays one comparison per record and no I/O. A run near saturation has far
     * more short episodes than fit, so the fuller the log, the longer an episode must have been to keep its place. */
    static final class Episodes {
        static final int CAPACITY=1<<16, FIELDS=5;
        final long[] data=new long[CAPACITY*FIELDS]; // first seq, start wall ns, records, max ns, end wall ns
        int used; long dropped; boolean open, kept;
        long floor() {
            return used<CAPACITY/2 ? 0 : used<CAPACITY/4*3 ? 10_000 : used<CAPACITY/8*7 ? 100_000 : 1_000_000;
        }
        void over(long seq, long wall, long ns) {
            if (!open) {
                open=true;
                // The episode before this one is complete: it gives its place back if it was too short.
                if (kept && data[(used-1)*FIELDS+3]<floor()) { used--; dropped++; }
                kept=used<CAPACITY;
                if (!kept) { dropped++; return; }
                int at=used++*FIELDS;
                data[at]=seq; data[at+1]=wall; data[at+2]=1; data[at+3]=ns; data[at+4]=wall;
            } else if (kept) {
                int at=(used-1)*FIELDS;
                data[at+2]++; data[at+3]=Math.max(data[at+3],ns); data[at+4]=wall;
            }
        }
    }
    /** The writer's loop. The long warm-up runs the same object code on scratch endpoints. */
    static final class Sender {
        final Endpoint endpoint; final byte[] data; final ByteBuffer buffer;
        final boolean live, probe; final long rate, durationNs, syncEveryNs;
        final Histogram syncHist=new Histogram(3);
        final Episodes episodes=new Episodes();
        long t0, start, sent, late, maxLate, nextSync, syncOverruns;
        String stoppedBy="count", syncOp;
        Sender(Endpoint endpoint, byte[] data, ByteBuffer buffer, boolean live, boolean probe, long rate,
               long durationNs, long syncEveryNs) {
            this.endpoint=endpoint; this.data=data; this.buffer=buffer; this.live=live; this.probe=probe;
            this.rate=rate; this.durationNs=durationNs; this.syncEveryNs=syncEveryNs;
        }
        long begin() { start=System.nanoTime(); nextSync=start+syncEveryNs; return start; }
        /** Sends records from..to; false once a probe's duration has passed. */
        boolean send(long from, long to) throws Exception {
            for (long i=from;i<to;i++) {
                long intended;
                if (probe) {
                    // An unpaced probe stores the actual send start, so reader latency is from the send.
                    intended=now();
                    if (i>0 && System.nanoTime()-start>=durationNs) { stoppedBy="duration"; return false; }
                } else if (live) {
                    intended=Math.addExact(t0, Math.multiplyExact(i,1_000_000_000L)/rate);
                    while (now()<intended) { endpoint.idle(); Thread.onSpinWait(); }
                    long wall=now(), delay=wall-intended;
                    if (delay>LATE_NS) { late++; episodes.over(i,wall,delay); } else episodes.open=false;
                    maxLate=Math.max(maxLate,delay);
                } else intended=0;
                buffer.putLong(0,i); buffer.putLong(8,intended);
                endpoint.send(data);
                sent++;
                if (syncEveryNs>0) {
                    long before=System.nanoTime();
                    if (before>=nextSync) {
                        syncOp=endpoint.periodicSync();
                        long after=System.nanoTime();
                        syncHist.recordValue(after-before);
                        // Fixed grid; grid points that passed during a slow sync are counted, not run late.
                        nextSync+=syncEveryNs;
                        while (nextSync<=after) { nextSync+=syncEveryNs; syncOverruns++; }
                    }
                }
            }
            return true;
        }
    }
    /** The reader's per-record check and histogram. */
    static final class Receiver implements Observer {
        final int payload; final boolean live; final long warmup; final Histogram hist;
        final Episodes episodes=new Episodes();
        long expected;
        Receiver(int payload, boolean live, long warmup, Histogram hist) {
            this.payload=payload; this.live=live; this.warmup=warmup; this.hist=hist;
        }
        public void record(long seq, long ts, int len) {
            if (seq!=expected || len!=payload) throw new IllegalStateException("record sequence/length mismatch");
            if (live && expected>=warmup) {
                long wall=now(), latency=wall-ts;
                if (latency<0) throw new IllegalStateException("wall clock moved backwards");
                hist.recordValue(latency);
                if (latency>SLOW_NS) episodes.over(seq,wall,latency); else episodes.open=false;
            }
            expected++;
        }
    }
    /** The reader's loop, also used by the warm-up. */
    static final class Poller {
        final Endpoint endpoint; final Receiver receiver; final Path done; final long deadline;
        long target;
        boolean known;
        Poller(Endpoint endpoint, Receiver receiver, long target, Path done, long deadline) {
            this.endpoint=endpoint; this.receiver=receiver; this.target=target; this.done=done; this.deadline=deadline;
            known=done==null;
        }
        /** Polls up to `polls` times; false when every record has arrived. */
        boolean poll(int polls) throws Exception {
            for (int n=0;n<polls;n++) {
                if (receiver.expected>=target) return false;
                boolean work=endpoint.poll(receiver);
                if (!work) {
                    Thread.onSpinWait();
                    if (!known && Files.exists(done)) {
                        target=Long.parseLong(Files.readString(done).trim());
                        known=true;
                    }
                }
                if ((!work || (receiver.expected&16383)==0) && System.nanoTime()>deadline)
                    throw new IllegalStateException("reader timed out");
            }
            return receiver.expected<target;
        }
    }
    static Map<String,Object> javaBlock(Map<String,Object> warmup, Endpoint endpoint, Episodes episodes) {
        Map<String,Object> block=new LinkedHashMap<>();
        block.put("warmup",warmup);
        block.put("tool",endpoint.settings());
        block.put("episodes",episodes.used);
        block.put("episodes_dropped",episodes.dropped);
        return block;
    }
    /** Writes the episodes and the measured window beside the JVM logs, after the measurement. Window times are
     * System.nanoTime() values, the clock of the logs' timenanos decoration. */
    static void writeEvents(Map<String,String> opts, String role, long threshold, long start, long measuredFrom,
                            long end, Episodes e) throws Exception {
        String path=opts.get("--events");
        if (path==null) return;
        Map<String,Object> m=new LinkedHashMap<>();
        m.put("role",role); m.put("threshold_ns",threshold);
        // The wall clock may be slewed during a long run: one anchor at the end is off by at most the slew.
        m.put("anchor_wall_ns",now()); m.put("anchor_nano",System.nanoTime());
        m.put("start_nano",start); m.put("measured_from_nano",measuredFrom); m.put("end_nano",end);
        m.put("fields",List.of("first_seq","start_wall_ns","records","max_ns","end_wall_ns"));
        List<long[]> rows=new ArrayList<>();
        for (int i=0;i<e.used;i++) rows.add(Arrays.copyOfRange(e.data,i*Episodes.FIELDS,(i+1)*Episodes.FIELDS));
        m.put("episodes",rows); m.put("episodes_dropped",e.dropped);
        Files.writeString(Path.of(path),JSON.writeValueAsString(m)+"\n");
    }
    public static void run(String[] args, Factory factory) throws Exception {
        var resultOut = System.out;
        System.setOut(System.err); // Third-party startup notices belong in children.log.
        String role = args[0];
        Map<String,String> opts = new HashMap<>();
        Set<String> allowed = Set.of("--dir","--control","--count","--payload","--rate","--warmup","--reader","--mode","--jit-warmup",
            "--duration-ms","--sync-every-ms",
            "--warmup-mode","--events","--micro-touch","--block-size");
        for (int i=1;i<args.length;i+=2) {
            if (!allowed.contains(args[i]) || i+1>=args.length || opts.put(args[i],args[i+1])!=null)
                throw new IllegalArgumentException("bad option " + args[i]);
        }
        Path dir = Path.of(opts.get("--dir"));
        Path control = Path.of(opts.getOrDefault("--control",dir.toString()));
        long count = Long.parseLong(opts.get("--count"));
        int payload = Integer.parseInt(opts.get("--payload"));
        long rate = Long.parseLong(opts.getOrDefault("--rate","100000"));
        long warmup = Long.parseLong(opts.getOrDefault("--warmup","0"));
        // sustained_probe: unpaced sends with a concurrent reader until --duration-ms passes or --count is reached.
        boolean probe = opts.get("--mode").equals("sustained_probe");
        boolean live = opts.get("--mode").equals("visible_latency") || probe;
        long durationNs = Long.parseLong(opts.getOrDefault("--duration-ms","0"))*1_000_000L;
        long syncEveryNs = Long.parseLong(opts.getOrDefault("--sync-every-ms","0"))*1_000_000L;
        if (count<=0 || rate<=0 || warmup<0 || warmup>=count || (payload!=64 && payload!=1024)
            || probe != (durationNs>0) || syncEveryNs<0 || (syncEveryNs>0 && !live))
            throw new IllegalArgumentException("invalid benchmark options");
        periodicSyncRequested = syncEveryNs>0;
        options = Map.copyOf(opts);
        boolean longWarmup = opts.getOrDefault("--warmup-mode","short").equals("long");
        long jitWarmup=Long.parseLong(opts.getOrDefault("--jit-warmup","100000"));
        Map<String,Object> warmupInfo = new LinkedHashMap<>();
        warmupInfo.put("mode",longWarmup?"long":"short");
        warmupInfo.put("records",longWarmup || !live ? jitWarmup : 0);
        // Warm the wall clock and HdrHistogram before opening the readiness barrier.
        Histogram hist = new Histogram(3);
        for (int i=0;i<10000;i++) { hist.recordValue(now() & 65535); }
        hist.reset();
        // Exercise the actual adapter hot path in this JVM before W1/W3 timing.
        // W1 uses a separate scratch queue; W3 reads the existing fixture without
        // consuming it, then reopens a fresh endpoint for the timed traversal.
        if (longWarmup) {
            Warmup.run(role,factory,dir,new Warmup.Spec(payload,rate,live,probe,syncEveryNs,jitWarmup),hist,warmupInfo);
        } else if (!live) {
            if (role.equals("writer")) {
                Path scratch=dir.resolveSibling(dir.getFileName()+".jit");
                Files.createDirectory(scratch);
                try (Endpoint warm=factory.open("writer",scratch)) {
                    byte[] bytes=new byte[payload+16];
                    for (long i=0;i<jitWarmup;i++) warm.send(bytes);
                }
            } else {
                try (Endpoint warm=factory.open("reader",dir)) {
                    long[] read={0};
                    Observer observer=(seq,ts,len)->read[0]++;
                    long deadline=System.nanoTime()+TIMEOUT;
                    while (read[0]<Math.min(jitWarmup,count)) {
                        warm.poll(observer);
                        if (System.nanoTime()>deadline) throw new IllegalStateException("JIT warmup timed out");
                    }
                }
            }
        }
        long replayStart = System.nanoTime();
        try (Endpoint endpoint = factory.open(role,dir)) {
            if (role.equals("writer")) {
                byte[] data = new byte[payload+16];
                Arrays.fill(data,(byte)0x5a);
                ByteBuffer buffer = ByteBuffer.wrap(data).order(ByteOrder.LITTLE_ENDIAN);
                long t0 = live ? barrier(control,"writer") : 0;
                endpoint.connected();
                if (live) while (now()<t0) Thread.onSpinWait();
                Sender sender=new Sender(endpoint,data,buffer,live,probe,rate,durationNs,syncEveryNs);
                sender.t0=t0;
                long start=sender.begin();
                for (long i=0;i<count && sender.send(i,Math.min(count,i+BATCH));i+=BATCH) { }
                long sent=sender.sent;
                long end=System.nanoTime();
                double seconds=(end-start)/1e9;
                if (live) {
                    Files.writeString(control.resolve("writer.done.tmp"),Long.toString(sent));
                    Files.move(control.resolve("writer.done.tmp"),control.resolve("writer.done"),
                        StandardCopyOption.ATOMIC_MOVE);
                }
                String sync=live ? null : endpoint.syncOperation();
                long before=System.nanoTime();
                if (sync!=null) endpoint.sync();
                Double flushSeconds=sync==null?null:(System.nanoTime()-before)/1e9;
                Map<String,Object> result=new LinkedHashMap<>();
                result.put("count",sent); result.put("planned_count",count); result.put("seconds",seconds);
                result.put("records_per_s",sent/seconds);
                result.put("payload_mb_per_s",sent*payload/seconds/1e6);
                result.put("achieved_records_per_s",sent/seconds); result.put("late_records",probe?null:sender.late);
                result.put("max_late_us",probe||!live?null:sender.maxLate/1e3);
                result.put("stopped_by",sender.stoppedBy);
                if (syncEveryNs>0) {
                    Map<String,Object> periodic=new LinkedHashMap<>();
                    periodic.put("operation",sender.syncOp); periodic.put("interval_ms",syncEveryNs/1_000_000L);
                    periodic.put("count",sender.syncHist.getTotalCount()); periodic.put("grid_overruns",sender.syncOverruns);
                    periodic.put("latency",sender.syncHist.getTotalCount()==0?emptyHistogram():histogram(sender.syncHist));
                    result.put("periodic_sync",periodic);
                } else result.put("periodic_sync",null);
                result.put("flush_operation",sync);
                result.put("flush_seconds",flushSeconds);
                result.put("java",javaBlock(warmupInfo,endpoint,sender.episodes));
                writeEvents(opts,"writer",LATE_NS,start,
                    !probe && live ? start+Math.multiplyExact(warmup,1_000_000_000L)/rate : start,end,sender.episodes);
                resultOut.println(JSON.writeValueAsString(result));
                if (live) {
                    long deadline=System.nanoTime()+TIMEOUT;
                    while (!Files.exists(control.resolve("reader.done"))) {
                        if (System.nanoTime()>deadline) throw new IllegalStateException("reader drain timed out");
                        Thread.sleep(1);
                    }
                }
            } else if (role.equals("reader")) {
                Receiver receiver=new Receiver(payload,live,warmup,hist);
                long t0 = live ? barrier(control,"reader") : 0;
                long pollStart = live ? System.nanoTime()+(t0-now()) : replayStart;
                // A probe's count is only a disk cap: its deadline follows the duration instead.
                long deadline=System.nanoTime() + TIMEOUT + (probe ? durationNs : count/rate*1_000_000_000L);
                // An unpaced probe learns its record count from writer.done.
                Poller poller=new Poller(endpoint,receiver,count,probe?control.resolve("writer.done"):null,deadline);
                while (poller.poll(BATCH)) { }
                long target=poller.target;
                if (receiver.expected!=target) throw new IllegalStateException("incomplete read");
                long end=System.nanoTime();
                double seconds=(end-replayStart)/1e9;
                Map<String,Object> result;
                if (probe) {
                    Map<String,Object> m=new LinkedHashMap<>();
                    m.put("count",target); m.put("latency_from","send start"); m.put("latency",histogram(hist));
                    result=m;
                } else result=live?histogram(hist):new LinkedHashMap<>(Map.of("count",count,"seconds",seconds,
                    "records_per_s",count/seconds,"payload_mb_per_s",count*payload/seconds/1e6));
                result.put("java",javaBlock(warmupInfo,endpoint,receiver.episodes));
                writeEvents(opts,"reader",SLOW_NS,pollStart,
                    live && !probe ? pollStart+Math.multiplyExact(warmup,1_000_000_000L)/rate : pollStart,end,receiver.episodes);
                resultOut.println(JSON.writeValueAsString(result));
                if (live) Files.write(control.resolve("reader.done"),new byte[0]);
            } else throw new IllegalArgumentException("expected writer/reader");
        }
    }
}
