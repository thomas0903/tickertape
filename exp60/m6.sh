# builds: setup.sh
cd; sleep 20
C="--duration 60s --warmup 10s --payload 1024 --checkpoint-every 1s --retain-bytes 68719476736 --readers mapped,safe:wait=default,safe:durable:wait=block --checkpoint-mode background --cpus 1,2,3,4,5,6"
for i in 1 2 3; do
 bash run1.sh m6-base-$i exp2 "-" -- $C --rate 304571
 bash run1.sh m6-rng-$i exp2 "TT_EXP_RANGED=1" -- $C --rate 304571
 bash run1.sh m6-rng8-$i exp2 "TT_EXP_RANGED=1 TT_EXP_WINDOW_KIB=8192" -- $C --rate 304571
 bash run1.sh m6-rng48-$i exp2 "TT_EXP_RANGED=1 TT_EXP_WINDOW_KIB=49152" -- $C --rate 304571
 bash run1.sh m6-lazyns-$i exp2 "lazytime TT_EXP_SPARE=nosync" -- $C --rate 304571
 bash run1.sh m6-lazyoff-$i exp2 "lazytime" -- $C --rate 304571
done
# class C: the finish round's commands (180 s, 4 CPUs) at its commit and at 1.0.0
for i in 1 2; do for b in finish main100; do
 bash run1.sh m6-c-$b-$i $b "-" -- --duration 180s --warmup 10s --payload 1024 --rate 250000 --checkpoint-every 1s --retain-bytes 68719476736 --readers mapped,safe:wait=default,safe:durable:wait=block --cpus 1,2,3,4 --checkpoint-mode background --helper-cpus 5-7
done; done
# traced: unpaced, both mounts
sudo mount -o remount,noatime,nolazytime /data
bash traced.sh t-unpaced main100 /data/tt --unpaced --duration 60s --warmup 10s --payload 1024 --checkpoint-every 1s --retain-bytes 68719476736 --readers mapped --checkpoint-mode background --cpus 1,2,3
sudo mount -o remount,noatime,lazytime /data
bash traced.sh t90-lazy main100 /data/tt --rate 304571 ${C/--duration 60s/--duration 60s}
sudo mount -o remount,noatime,nolazytime /data
touch r/m6.DONE
