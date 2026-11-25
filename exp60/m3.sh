# sustained, background checkpoints: spare off/sync/nosync at steady-50 and steady-90 of the published probe, and unpaced
R50=${R50:-169206}; R90=${R90:-304571}; N=${N:-5}
C="--duration 60s --warmup 10s --payload 1024 --checkpoint-every 1s --retain-bytes 68719476736 --readers mapped,safe:wait=default,safe:durable:wait=block --checkpoint-mode background --cpus ${CPUS:-1,2,3,4,5,6}"
for i in $(seq 1 $N); do
 for s in off sync nosync; do
  bash run1.sh m3-s50-$s-$i exp "TT_EXP_SPARE=$s" -- $C --rate $R50
  bash run1.sh m3-s90-$s-$i exp "TT_EXP_SPARE=$s" -- $C --rate $R90
 done
done
touch r/m3.DONE
