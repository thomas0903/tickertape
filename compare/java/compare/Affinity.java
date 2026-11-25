package compare;

import java.nio.file.*;
import java.util.*;

/** Pins the measured thread and reads back the affinity in effect. Without native code in the harness: the kernel's
 * id of the calling thread is in /proc/thread-self, and taskset (util-linux) changes a thread by id. */
final class Affinity {
    private static final boolean LINUX = System.getProperty("os.name").equals("Linux");

    /** Pins the calling thread to the CPU (null: leaves it alone) or fails. The JVM's other threads are not changed. */
    static void pin(String cpu, Path control, String role) throws Exception {
        if (cpu==null) return;
        if (!LINUX) throw new IllegalStateException("CPU pinning is unsupported on this OS");
        announce(control,role+".tid");
        String[] command={"taskset","-p","-c",Integer.toString(Integer.parseInt(cpu)),tid()};
        // From another thread: the JDK starts its "process reaper" thread from the thread that starts the process,
        // while taskset already runs, and it would inherit the new CPU.
        int[] status={-1};
        Exception[] failure={null};
        Thread starter=new Thread(() -> {
            try {
                status[0]=new ProcessBuilder(command).redirectOutput(ProcessBuilder.Redirect.DISCARD)
                    .redirectError(ProcessBuilder.Redirect.INHERIT).start().waitFor();
            } catch (Exception e) { failure[0]=e; }
        },"taskset-starter");
        starter.start();
        starter.join();
        if (status[0]!=0) throw new IllegalStateException("cannot pin to CPU " + cpu,failure[0]);
    }

    /** Tells the runner's Keeper about this thread: role.tid before it is pinned, role.measured after the read-back. */
    private static void announce(Path control, String name) throws Exception {
        Files.writeString(control.resolve(name+".tmp"),tid());
        Files.move(control.resolve(name+".tmp"),control.resolve(name),StandardCopyOption.ATOMIC_MOVE);
    }

    private static String tid() throws Exception {
        return Files.readSymbolicLink(Path.of("/proc/thread-self")).getFileName().toString();
    }

    /** As report() in benches/src/affinity.rs; the endpoint must still be open. Then tells the Keeper that the
     * measurement is over: the JVM's shutdown hooks start later, from this pinned thread. */
    static Map<String,Object> report(String cpu, Path control, String role) throws Exception {
        String os=System.getProperty("os.name");
        if (!LINUX) return Map.of("unsupported","affinity: unsupported on " + (os.startsWith("Mac") ? "macOS" : os));
        String tid=tid();
        Map<String,Object> result=thread(Path.of("/proc/thread-self"));
        String stat=Files.readString(Path.of("/proc/thread-self/stat"));
        // Field 39, processor; the name in field 2 may contain spaces, so count from the last parenthesis.
        result.put("last_cpu",Integer.parseInt(stat.substring(stat.lastIndexOf(')')+2).split(" ")[36]));
        result.put("tid",Long.parseLong(tid));
        if (((List<?>) result.get("cpus_allowed")).size()==1) Thread.sleep(50);
        List<Object> others=new ArrayList<>();
        try (DirectoryStream<Path> tasks=Files.newDirectoryStream(Path.of("/proc/self/task"))) {
            for (Path task : tasks) {
                if (task.getFileName().toString().equals(tid)) continue;
                try { others.add(thread(task)); }
                catch (java.io.IOException e) {
                    // A thread that exited between the listing and the read is gone, not misplaced.
                    if (Files.exists(task)) throw e;
                }
            }
        }
        result.put("other_threads",others);
        if (cpu!=null) announce(control,role+".measured");
        return result;
    }

    private static Map<String,Object> thread(Path task) throws java.io.IOException {
        Map<String,Object> result=new LinkedHashMap<>();
        for (String line : Files.readAllLines(task.resolve("status"))) {
            if (line.startsWith("Name:")) result.put("name",line.substring(5).trim());
            if (line.startsWith("Cpus_allowed_list:")) {
                List<Integer> cpus=new ArrayList<>();
                for (String part : line.substring(18).trim().split(",")) {
                    String[] range=part.split("-");
                    for (int cpu=Integer.parseInt(range[0]);cpu<=Integer.parseInt(range[range.length-1]);cpu++) cpus.add(cpu);
                }
                result.put("cpus_allowed",cpus);
            }
        }
        return result;
    }
}
