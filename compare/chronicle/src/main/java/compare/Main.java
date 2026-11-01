package compare;

import net.openhft.chronicle.queue.*;
import net.openhft.chronicle.queue.impl.single.SingleChronicleQueueBuilder;
import net.openhft.chronicle.bytes.Bytes;
import net.openhft.chronicle.wire.DocumentContext;
import java.nio.file.Path;

public final class Main {
    static final class QueueEndpoint implements Harness.Endpoint {
        final ChronicleQueue queue;
        final ExcerptAppender appender;
        final ExcerptTailer tailer;
        QueueEndpoint(String role, Path dir) {
            queue = SingleChronicleQueueBuilder.binary(dir.toFile()).build();
            appender = role.equals("writer") ? queue.createAppender() : null;
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
                // Touch the last payload byte without copying the mapped document.
                b.readByte(b.readPosition()+length-17);
                observer.record(seq,ts,length-16);
                return true;
            }
        }
        public String syncOperation() { return "ExcerptAppender.sync (MS_ASYNC submission)"; }
        public void sync() { appender.sync(); }
        public void close() { queue.close(); }
    }
    public static void main(String[] args) throws Exception { Harness.run(args,QueueEndpoint::new); }
}
