A="--duration 60s --warmup 10s --payload 1024 --checkpoint-every 1s --retain-bytes 68719476736 --readers mapped,safe:wait=default,safe:durable:wait=block --checkpoint-mode background --cpus 1,2,3,4,5,6 --rate 304571"
for i in 1 2 3; do
 bash run1.sh m1-base-$i exp "-" -- $A
 bash run1.sh m1-lazy-$i exp "lazytime" -- $A
 bash run1.sh m1-w8m-$i exp "TT_EXP_WINDOW_KIB=8192" -- $A
 bash run1.sh m1-w48m-$i exp "TT_EXP_WINDOW_KIB=49152" -- $A
done
touch r/m1.DONE
