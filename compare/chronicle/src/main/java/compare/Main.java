package compare;

import net.openhft.chronicle.queue.*;
import net.openhft.chronicle.queue.impl.single.SingleChronicleQueueBuilder;
import net.openhft.chronicle.bytes.Bytes;
import net.openhft.chronicle.bytes.SyncMode;
import net.openhft.chronicle.wire.DocumentContext;
import net.openhft.chronicle.queue.util.MicroTouched;
import java.nio.file.Path;
import java.util.*;

public final class Main {
    static final class QueueEndpoint implements Harness.Endpoint {
        final ChronicleQueue queue;
        final ExcerptAppender appender;
        final ExcerptTailer tailer;
        /** The open-source appender's substitute for the Enterprise pretoucher; null when switched off. */
        final MicroTouched microTouch;
        final long blockSize;
        final boolean sendStart=Harness.sendStartRequested();
        long attempts; // Calls that reached a new page; the touch is only made if that page is mapped.
        QueueEndpoint(String role, Path dir) {
            // Periodic durability (SD) uses msync(MS_SYNC) through SyncMode.SYNC; otherwise the default.
            SingleChronicleQueueBuilder builder = SingleChronicleQueueBuilder.binary(dir.toFile());
            if (Harness.periodicSyncRequested) builder.syncMode(SyncMode.SYNC);
            // The warm-up has to cross chunks, whatever the measured queue uses: its full rounds keep Chronicle's
            // default of 64 MiB and its fresh rounds use chunks small enough for their few records.
            String block = Harness.options.getOrDefault("--block-size","default");
            if (Harness.phase==Harness.Phase.FRESH_WARMUP) builder.blockSize(256L<<10);
            else if (Harness.phase==Harness.Phase.MEASURED && !block.equals("default")) builder.blockSize(Long.parseLong(block));
            blockSize = builder.blockSize();
            queue = builder.build();
            appender = role.equals("writer") ? queue.createAppender() : null;
            microTouch = appender!=null && Harness.options.getOrDefault("--micro-touch","off").equals("on")
                ? (MicroTouched)appender : null;
            tailer = role.equals("reader") ? queue.createTailer().toStart() : null;
        }
        public void send(byte[] bytes) {
            try (DocumentContext dc=appender.writingDocument()) { dc.wire().bytes().write(bytes); }
        }
        public boolean poll(Harness.Observer observer) {
            try (DocumentContext dc=tailer.readingDocument()) {
                if (!dc.isPresent()) return false;
                Bytes<?> b=dc.wire().bytes();
                int length=(int)b.readRemaining();
                long seq=b.readLong(), ts=b.readLong();
                // --send-start: the first 8 application bytes, read in place.
                long sent=sendStart ? b.readLong(b.readPosition()) : 0;
                // Touch the last payload byte without copying the mapped document.
                b.readByte(b.readPosition()+length-17);
                observer.record(seq,ts,sent,length-16);
                return true;
            }
        }
        public void idle() { if (microTouch!=null && microTouch.microTouch()) attempts++; }
        public Map<String,Object> settings() {
            Map<String,Object> m=new LinkedHashMap<>();
            m.put("block_size",blockSize);
            if (appender!=null) { m.put("micro_touch",microTouch!=null); m.put("micro_touch_attempts",attempts); }
            return m;
        }
        public String syncOperation() { return "ExcerptAppender.sync (MS_ASYNC submission)"; }
        public void sync() { appender.sync(); }
        public String periodicSync() { appender.sync(); return "ExcerptAppender.sync with SyncMode.SYNC (msync MS_SYNC)"; }
        public void close() { queue.close(); }
    }
    public static void main(String[] args) throws Exception { Harness.run(args,QueueEndpoint::new); }
}
