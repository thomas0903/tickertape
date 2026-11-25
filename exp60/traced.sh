#!/usr/bin/env bash
# traced.sh <name> <build> <dir> <tt-sustained args...>: one run with events, offcpu, stalls tracers
N=$1; B=$2; D=$3; shift 3
O=$HOME/r/$N; rm -rf $O $D; mkdir -p $O; sync; sleep 2
~/b/$B/tt-sustained run --dir $D --out $O/out --json "$@" > $O/stdout 2> $O/stderr &
R=$!
for i in $(seq 1 400); do W=$(pgrep -f '^[^ ]*tt-sustained writer ' | head -1); [ -n "$W" ] && break; sleep 0.05; done
sudo /opt/bt/bpftrace ~/diag/offcpu.bt $W ${MIN_MS:-3} 86400 > $O/offcpu.txt 2>&1 &
sleep 2
sudo /opt/bt/bpftrace ~/diag/events.bt $W 86400 > $O/events.txt 2>&1 &
sleep 2
sudo /opt/bt/bpftrace ~/stalls.bt $W ${MIN_US:-500} 86400 > $O/stalls.txt 2>&1 &
sleep 2; sudo /opt/bt/bpftrace ~/bio.bt 86400 > $O/bio.txt 2>&1 &
sleep 2; ~/diag/find-threads.sh > $O/threads.txt 2>&1
wait $R; echo "exit $?" >> $O/stderr
sudo pkill -INT bpftrace; sleep 3; rm -rf $D

touch $O/DONE
