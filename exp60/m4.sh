# unpaced (rolls in an overloaded device): base, spare, lazytime, both
N=${N:-4}
C="--duration 60s --warmup 10s --payload 1024 --checkpoint-every 1s --retain-bytes 68719476736 --readers mapped,safe:wait=default --checkpoint-mode background --cpus ${CPUS:-1,2,3,4,5,6} --unpaced"
for i in $(seq 1 $N); do
  bash run1.sh m4-off-$i exp "TT_EXP_SPARE=off" -- $C
  bash run1.sh m4-nosync-$i exp "TT_EXP_SPARE=nosync" -- $C
  bash run1.sh m4-lazy-$i exp "lazytime TT_EXP_SPARE=off" -- $C
  bash run1.sh m4-lazynosync-$i exp "lazytime TT_EXP_SPARE=nosync" -- $C
done
touch r/m4.DONE
