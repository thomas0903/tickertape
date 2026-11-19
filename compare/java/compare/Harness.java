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
        @Override void close() throws Exception;
    }
    public interface Factory { Endpoint open(String role, Path dir) throws Exception; }
    static final ObjectMapper JSON = new ObjectMapper();
    static final long TIMEOUT = 120_000_000_000L;
    /** Set before endpoints open when the run requests periodic durability (--sync-every-ms). */
    public static volatile boolean periodicSyncRequested = false;
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
    public static void run(String[] args, Factory factory) throws Exception {
        var resultOut = System.out;
        System.setOut(System.err); // Third-party startup notices belong in children.log.
        String role = args[0];
        Map<String,String> opts = new HashMap<>();
        Set<String> allowed = Set.of("--dir","--control","--count","--payload","--rate","--warmup","--reader","--mode","--jit-warmup",
            "--duration-ms","--sync-every-ms");
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
        // Warm the wall clock and HdrHistogram before opening the readiness barrier.
        Histogram hist = new Histogram(3);
        for (int i=0;i<10000;i++) { hist.recordValue(now() & 65535); }
        hist.reset();
        // Exercise the actual adapter hot path in this JVM before W1/W3 timing.
        // W1 uses a separate scratch queue; W3 reads the existing fixture without
        // consuming it, then reopens a fresh endpoint for the timed traversal.
        if (!live) {
            long jitWarmup=Long.parseLong(opts.getOrDefault("--jit-warmup","100000"));
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
                long late=0, maxLate=0, start=0, sent=0, syncOverruns=0;
                String stoppedBy="count", syncOp=null;
                Histogram syncHist=new Histogram(3);
                if (probe) while (now()<t0) Thread.onSpinWait();
                long nextSync=System.nanoTime()+syncEveryNs;
                for (long i=0;i<count;i++) {
                    long intended;
                    if (probe) {
                        // An unpaced probe stores the actual send start, so reader latency is from the send.
                        intended=now();
                        if (i>0 && System.nanoTime()-start>=durationNs) { stoppedBy="duration"; break; }
                    } else if (live) {
                        intended=Math.addExact(t0, Math.multiplyExact(i,1_000_000_000L)/rate);
                        while (now()<intended) Thread.onSpinWait();
                        long delay=now()-intended;
                        if (delay>1000) late++;
                        maxLate=Math.max(maxLate,delay);
                    } else intended=0;
                    if (i==0) start=System.nanoTime();
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
                double seconds=(System.nanoTime()-start)/1e9;
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
                result.put("achieved_records_per_s",sent/seconds); result.put("late_records",probe?null:late);
                result.put("max_late_us",probe||!live?null:maxLate/1e3);
                result.put("stopped_by",stoppedBy);
                if (syncEveryNs>0) {
                    Map<String,Object> periodic=new LinkedHashMap<>();
                    periodic.put("operation",syncOp); periodic.put("interval_ms",syncEveryNs/1_000_000L);
                    periodic.put("count",syncHist.getTotalCount()); periodic.put("grid_overruns",syncOverruns);
                    periodic.put("latency",syncHist.getTotalCount()==0?emptyHistogram():histogram(syncHist));
                    result.put("periodic_sync",periodic);
                } else result.put("periodic_sync",null);
                result.put("flush_operation",sync);
                result.put("flush_seconds",flushSeconds);
                resultOut.println(JSON.writeValueAsString(result));
                if (live) {
                    long deadline=System.nanoTime()+TIMEOUT;
                    while (!Files.exists(control.resolve("reader.done"))) {
                        if (System.nanoTime()>deadline) throw new IllegalStateException("reader drain timed out");
                        Thread.sleep(1);
                    }
                }
            } else if (role.equals("reader")) {
                long[] expected={0};
                Observer observe=(seq,ts,len)-> {
                    if (seq!=expected[0] || len!=payload) throw new IllegalStateException("record sequence/length mismatch");
                    if (live && expected[0]>=warmup) {
                        long latency=now()-ts;
                        if (latency<0) throw new IllegalStateException("wall clock moved backwards");
                        hist.recordValue(latency);
                    }
                    expected[0]++;
                };
                if (live) barrier(control,"reader");
                // A probe's count is only a disk cap: its deadline follows the duration instead.
                long deadline=System.nanoTime() + TIMEOUT + (probe ? durationNs : count/rate*1_000_000_000L);
                // An unpaced probe learns its record count from writer.done.
                long target=count;
                boolean known=!probe;
                while (expected[0]<target) {
                    boolean work=endpoint.poll(observe);
                    if (!work) {
                        Thread.onSpinWait();
                        if (!known && Files.exists(control.resolve("writer.done"))) {
                            target=Long.parseLong(Files.readString(control.resolve("writer.done")).trim());
                            known=true;
                        }
                    }
                    if ((!work || (expected[0]&16383)==0) && System.nanoTime()>deadline)
                        throw new IllegalStateException("reader timed out");
                }
                if (expected[0]!=target) throw new IllegalStateException("incomplete read");
                double seconds=(System.nanoTime()-replayStart)/1e9;
                Object result;
                if (probe) {
                    Map<String,Object> m=new LinkedHashMap<>();
                    m.put("count",target); m.put("latency_from","send start"); m.put("latency",histogram(hist));
                    result=m;
                } else result=live?histogram(hist):Map.of("count",count,"seconds",seconds,
                    "records_per_s",count/seconds,"payload_mb_per_s",count*payload/seconds/1e6);
                resultOut.println(JSON.writeValueAsString(result));
                if (live) Files.write(control.resolve("reader.done"),new byte[0]);
            } else throw new IllegalArgumentException("expected writer/reader");
        }
    }
}
