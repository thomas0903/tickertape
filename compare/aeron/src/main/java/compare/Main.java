package compare;

import io.aeron.*;
import io.aeron.driver.*;
import org.agrona.concurrent.*;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.file.Path;

/** IPC only: these term buffers are a transport, never a persistent journal. */
public final class Main {
    static final class IpcEndpoint implements Harness.Endpoint {
        final MediaDriver driver;
        final Aeron aeron;
        final Publication publication;
        final Subscription subscription;
        final UnsafeBuffer buffer=new UnsafeBuffer(ByteBuffer.allocateDirect(1040));
        IpcEndpoint(String role, Path dir) {
            String directory=dir.resolve("driver").toString();
            driver=role.equals("writer") ? MediaDriver.launch(new MediaDriver.Context()
                .aeronDirectoryName(directory).dirDeleteOnStart(true).dirDeleteOnShutdown(true)
                .threadingMode(ThreadingMode.DEDICATED)
                .conductorIdleStrategy(new BusySpinIdleStrategy())
                .senderIdleStrategy(new NoOpIdleStrategy()).receiverIdleStrategy(new NoOpIdleStrategy())) : null;
            aeron=Aeron.connect(new Aeron.Context().aeronDirectoryName(directory));
            publication=role.equals("writer")?aeron.addExclusivePublication("aeron:ipc?term-length=16m",1001):null;
            subscription=role.equals("reader")?aeron.addSubscription("aeron:ipc?term-length=16m",1001):null;
        }
        public void connected() {
            long deadline=System.nanoTime()+Harness.TIMEOUT;
            while (!publication.isConnected()) {
                if (System.nanoTime()>deadline) throw new IllegalStateException("no subscriber");
                Thread.onSpinWait();
            }
        }
        public void send(byte[] bytes) {
            buffer.putBytes(0,bytes);
            long deadline=System.nanoTime()+Harness.TIMEOUT;
            long result;
            while ((result=publication.offer(buffer,0,bytes.length))<0) {
                if (result==Publication.CLOSED || result==Publication.MAX_POSITION_EXCEEDED || System.nanoTime()>deadline)
                    throw new IllegalStateException("offer failed " + result);
                Thread.onSpinWait();
            }
        }
        public boolean poll(Harness.Observer observer) {
            return subscription.poll((data,offset,length,header)-> {
                if (length<16) throw new IllegalStateException("short message");
                data.getByte(offset+length-1);
                observer.record(data.getLong(offset,ByteOrder.LITTLE_ENDIAN),
                    data.getLong(offset+8,ByteOrder.LITTLE_ENDIAN),length-16);
            },256)>0;
        }
        public void close() {
            if (publication!=null) publication.close();
            if (subscription!=null) subscription.close();
            aeron.close();
            if (driver!=null) driver.close();
        }
    }
    public static void main(String[] args) throws Exception { Harness.run(args,IpcEndpoint::new); }
}
