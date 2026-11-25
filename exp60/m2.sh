# ADR 0023 conditions: tt-latency 100,000/s, 1,024 B, mapped reader, 11 s; spare off/sync/nosync interleaved
N=${1:-30}; mkdir -p r; sudo mount -o remount,noatime,nolazytime /data
for i in $(seq 1 $N); do for s in off sync nosync; do
 rm -rf /data/lat; sync; sleep 1
 TT_EXP_SPARE=$s ~/b/exp/tt-latency run --dir /data/lat --rate 100000 --count 1100000 --warmup 100000 --payload 1024 --reader mapped --json 2>>r/m2.err | sed "s/^/$s $i /" >> r/m2.jsonl
done; done; rm -rf /data/lat; touch r/m2.DONE
