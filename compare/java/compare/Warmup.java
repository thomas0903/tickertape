package compare;

import org.HdrHistogram.Histogram;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.CountDownLatch;
import java.util.stream.Stream;

/** The long JVM warm-up (issue #56): runs the measured loops on scratch endpoints, in the JVM that measures next.
 *
 * The full rounds send enough records to cross what a run meets rarely: Chronicle's index arrays (every 1,048,576
 * records) and 64 MiB chunks, Aeron's 16 MiB terms, and the reader's histogram branch. The fresh rounds after them
 * repeat an endpoint's first records, which happen once per endpoint. HotSpot compiles a path it has not seen into
 * an uncommon trap, so a path first taken in the measured window deoptimises the hot methods there. Both kinds of
 * round repeat because one crossing only starts the recompilation that includes it, and because HotSpot keeps
 * compiled code with some kinds of trap until the fourth hit (PerBytecodeTrapLimit).
 *
 * Nothing of the warm-up is kept for the measurement except the JVM's own state: every round uses a new scratch
 * directory beside the data directory, deleted again, and the JVM is settled (garbage collected, `sync`) before
 * the caller announces readiness or starts its timer. */
final class Warmup {
    record Spec(int payload, long rate, boolean live, boolean probe, long syncEveryNs, long records) { }
    static final int FULL_ROUNDS = 3, FRESH_ROUNDS = 8;
    static final long FRESH_RECORDS = 20_000;
    /** Longest paced latency plus a margin: the histogram never has to grow in the measured window. */
    static final long HISTOGRAM_NS = 600_000_000_000L;

    private final Harness.Factory factory;
    private final Path dir;
    private final Spec spec;
    private Warmup(Harness.Factory factory, Path dir, Spec spec) { this.factory=factory; this.dir=dir; this.spec=spec; }

    static void run(String role, Harness.Factory factory, Path dir, Spec spec, Histogram measured,
                    Map<String,Object> info) throws Exception {
        long begin=System.nanoTime();
        measured.recordValue(HISTOGRAM_NS);
        measured.reset();
        // Exercise the episode log until it overflows, which a run near saturation does.
        Harness.Episodes log=Harness.episodes();
        for (int i=0;log!=null && i<Harness.Episodes.CAPACITY*4;i++) {
            long ns=(i&1)==0 ? 2_000 : 2_000_000;
            log.open=false; log.over(i,i,ns); log.over(i,i,ns); log.over(i,i,ns+1);
        }
        Warmup warmup=new Warmup(factory,dir,spec);
        try {
            for (int round=0;round<FULL_ROUNDS+FRESH_ROUNDS;round++) {
                boolean full=round<FULL_ROUNDS;
                Harness.phase=full ? Harness.Phase.FULL_WARMUP : Harness.Phase.FRESH_WARMUP;
                long records=full ? spec.records : Math.min(spec.records,FRESH_RECORDS);
                Path scratch=dir.resolveSibling(dir.getFileName()+".warmup-"+role+"-"+round);
                Files.createDirectory(scratch);
                try {
                    if (role.equals("writer")) warmup.writerRound(scratch,records);
                    else if (spec.live) warmup.liveReaderRound(scratch,records);
                    else warmup.replayRound(scratch,records);
                } finally { delete(scratch); }
                Thread.sleep(full ? 300 : 100); // Lets queued compilations finish before the next round runs on them.
            }
        } finally { Harness.phase=Harness.Phase.MEASURED; }
        System.gc();
        new ProcessBuilder("sync").inheritIO().redirectOutput(ProcessBuilder.Redirect.DISCARD).start().waitFor();
        info.put("full_rounds",FULL_ROUNDS);
        info.put("fresh_rounds",FRESH_ROUNDS);
        info.put("seconds",(System.nanoTime()-begin)/1e9);
    }
    private Harness.Sender sender(Harness.Endpoint endpoint) {
        byte[] data=new byte[spec.payload+16];
        Arrays.fill(data,(byte)0x5a);
        // A run syncs once a second; a warm-up of a few seconds needs more than one to have taken that path.
        long syncNs=Math.min(spec.syncEveryNs,200_000_000L);
        return new Harness.Sender(endpoint,data,ByteBuffer.wrap(data).order(ByteOrder.LITTLE_ENDIAN),spec.live,spec.probe,
            spec.rate,Harness.BARRIER_TIMEOUT,syncNs);
    }
    /** Paced at the run's rate, then a backlog (every send late), then paced again; unpaced modes just send. */
    private void send(Harness.Sender sender, long records) throws Exception {
        // At most a quarter of the records and 50 ms (fresh rounds) or 250 ms (full rounds) at the run's rate.
        long paced=Math.max(1,Math.min(records/4,records>FRESH_RECORDS ? spec.rate/4 : spec.rate/20));
        sender.t0=Harness.now();
        sender.begin();
        batches(sender,0,paced);
        sender.t0=Harness.now()-100_000_000_000L-at(paced);
        batches(sender,paced,records-paced);
        sender.t0=Harness.now()-at(records-paced);
        batches(sender,records-paced,records);
    }
    private long at(long record) { return Math.multiplyExact(record,1_000_000_000L)/spec.rate; }
    private static void batches(Harness.Sender sender, long from, long to) throws Exception {
        for (long i=from;i<to && sender.send(i,Math.min(to,i+Harness.BATCH));i+=Harness.BATCH) { }
    }
    private void writerRound(Path scratch, long records) throws Exception {
        try (Harness.Endpoint writer=factory.open("writer",scratch)) {
            CountDownLatch stop=new CountDownLatch(1);
            Task drain=!writer.needsPeer() ? null : new Task("warmup-drain",()-> {
                try (Harness.Endpoint reader=factory.open("reader",scratch)) {
                    Harness.Observer ignore=(seq,ts,len)-> { };
                    // A pause every 100 ms, so that the writer has met back pressure before the measurement does.
                    long pause=System.nanoTime();
                    while (stop.getCount()>0) {
                        if (!reader.poll(ignore)) Thread.onSpinWait();
                        if (System.nanoTime()-pause>100_000_000L) { Thread.sleep(20); pause=System.nanoTime(); }
                    }
                }
            });
            try {
                writer.connected();
                send(sender(writer),records);
            } finally { stop.countDown(); }
            if (drain!=null) drain.finish();
        }
    }
    private void liveReaderRound(Path scratch, long records) throws Exception {
        CountDownLatch opened=new CountDownLatch(1), subscribed=new CountDownLatch(1), read=new CountDownLatch(1);
        Path done=scratch.resolve("warmup.done");
        Task feed=new Task("warmup-feed",()-> {
            try (Harness.Endpoint writer=factory.open("writer",scratch)) {
                opened.countDown();
                subscribed.await();
                Thread.sleep(20); // As in a run, the reader first polls an empty endpoint.
                writer.connected();
                Harness.Sender sender=sender(writer);
                send(sender,records);
                Path tmp=scratch.resolve("warmup.done.tmp");
                Files.writeString(tmp,Long.toString(sender.sent));
                Files.move(tmp,done,StandardCopyOption.ATOMIC_MOVE);
                read.await();
            } finally { opened.countDown(); }
        });
        opened.await();
        try (Harness.Endpoint reader=factory.open("reader",scratch)) {
            subscribed.countDown();
            Harness.Poller poller=new Harness.Poller(reader,receiver(records),records,spec.probe?done:null,
                System.nanoTime()+Harness.BARRIER_TIMEOUT);
            while (poller.poll(Harness.BATCH)) feed.check();
        } finally { read.countDown(); subscribed.countDown(); }
        feed.finish();
    }
    private void replayRound(Path scratch, long records) throws Exception {
        try (Harness.Endpoint writer=factory.open("writer",scratch)) { send(sender(writer),records); }
        // A pass over the 64 B records crosses a 64 MiB chunk once, so one pass per round is too few.
        for (int pass=0;pass<4;pass++) {
            try (Harness.Endpoint reader=factory.open("reader",scratch)) {
                Harness.Poller poller=new Harness.Poller(reader,receiver(records),records,null,System.nanoTime()+Harness.BARRIER_TIMEOUT);
                while (poller.poll(Harness.BATCH)) { }
            }
        }
    }
    /** Like a run, the first tenth of the records stays out of the histogram; a probe records all of them. */
    private Harness.Receiver receiver(long records) {
        return new Harness.Receiver(spec.payload,spec.live,spec.probe ? 0 : records/10,new Histogram(3));
    }
    private static void delete(Path root) throws Exception {
        List<Path> paths;
        try (Stream<Path> walk=Files.walk(root)) { paths=walk.sorted(Comparator.reverseOrder()).toList(); }
        for (Path path : paths) Files.deleteIfExists(path);
    }
    /** A helper thread whose failure fails the warm-up. */
    private static final class Task extends Thread {
        interface Body { void run() throws Exception; }
        private final Body body;
        private volatile Throwable failure;
        Task(String name, Body body) { super(name); this.body=body; setDaemon(true); start(); }
        @Override public void run() { try { body.run(); } catch (Throwable t) { failure=t; } }
        void check() { if (failure!=null) throw new IllegalStateException(getName()+" failed",failure); }
        void finish() throws Exception {
            join(Harness.BARRIER_TIMEOUT/1_000_000);
            check();
            if (isAlive()) throw new IllegalStateException(getName()+" did not stop");
        }
    }
}
