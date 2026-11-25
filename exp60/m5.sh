# helper on the writer's CPU: does the maximum follow the scheduler's base slice?
N=${N:-3}; S=/sys/kernel/debug/sched/base_slice_ns; D=$(sudo cat $S)
C="--duration 60s --warmup 10s --payload 1024 --checkpoint-every 1s --retain-bytes 68719476736 --readers mapped,safe:wait=default,safe:durable:wait=block --checkpoint-mode background --cpus ${CPUS:-1,2,3,4,5,6} --rate ${R50:-169206} --helper-cpus inherited"
for i in $(seq 1 $N); do for b in 700000 2800000 8000000; do
  echo $b | sudo tee $S >/dev/null; bash run1.sh m5-slice$b-$i exp "-" -- $C
done; done; echo $D | sudo tee $S; touch r/m5.DONE
