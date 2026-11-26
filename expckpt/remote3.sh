#!/usr/bin/env bash
# Issue #62: what prefaulted, unwritten pages add to a checkpoint. Runs on a benchmark VM after build.sh.
# Env overrides for a local smoke test: LOCAL=1 (no mkfs; journals under $JROOT), DEVPATH, DUR, WARM, ROUNDS, FSLIST.
set -uo pipefail
OUT="${OUT:-$HOME/ckpt}"; mkdir -p "$OUT"; exec >>"$OUT/${LOG_FILE:-remote.log}" 2>&1
BIN="${BIN:-$HOME/bin}"; HERE="$(cd "$(dirname "$0")" && pwd)"
DUR="${DUR:-20s}"; WARM="${WARM:-3s}"; ROUNDS="${ROUNDS:-3}"; FSLIST="${FSLIST:-ext4 xfs}"
DEVPATH="${DEVPATH:-$(ls /dev/disk/by-id/google-local-nvme-ssd-* | grep -v part | head -1)}"
DEV="$(basename "$(readlink -f "$DEVPATH")")"; DEVBASE="$(lsblk -no PKNAME "/dev/$DEV" 2>/dev/null | head -1)"
STAT="/sys/class/block/$DEV/stat"
KD="/dev/${DEVBASE:-$DEV}"; MAJ=$((16#$(stat -c %t "$KD"))); MIN=$((16#$(stat -c %T "$KD"))); KDEV=$(( (MAJ << 20) | MIN ))
# First logical CPU of physical cores 1, 2 and 3: writer, reader, helper threads (core 0 for everything else).
CPUS=$(lscpu -p=CPU,CORE | grep -v '^#' | sort -t, -k2,2n -k1,1n | awk -F, '!s[$2]++ {print $1}' | sed -n '2,4p' | paste -sd, -)
W=$(cut -d, -f1 <<<"$CPUS"); R=$(cut -d, -f2 <<<"$CPUS"); H=$(cut -d, -f3 <<<"$CPUS")
echo "$(date -u +%FT%TZ) start dev=$DEV kdev=$KDEV cpus writer=$W reader=$R helpers=$H"
{
  date -u; uname -a; cat /etc/os-release | head -2; lscpu; lscpu -e; free -b; lsblk -o NAME,SIZE,MODEL,ROTA
  cat "/sys/class/block/${DEVBASE:-$DEV}/queue/scheduler" 2>/dev/null; cat "/sys/class/block/${DEVBASE:-$DEV}/queue/wbt_lat_usec" 2>/dev/null
  sysctl vm.dirty_background_ratio vm.dirty_ratio vm.dirty_expire_centisecs vm.dirty_writeback_centisecs vm.dirty_background_bytes vm.dirty_bytes
  getconf PAGESIZE; bpftrace --version; cat /sys/kernel/mm/transparent_hugepage/enabled; systemd-detect-virt
  cat "$OUT/binaries.txt" 2>/dev/null
} > "$OUT/${ENV_FILE:-environment.txt}" 2>&1

# name binary env extra-args
CONFIGS=(
  "A|main||"
  "C|main||--no-prefault"
  "D|exp|TT_EXP_NO_POPULATE=1|"
  "E|exp|TT_EXP_RANGED=1|"
)
RATES=("idle|1|64" "r20k|20000|64" "r200k|200000|64" "k1|20000|1024")

run_one() { # fs round rate-spec config-spec traced checkpoints(yes|no)
  local fs=$1 round=$2 rs=$3 cs=$4 traced=$5 ck=$6
  IFS='|' read -r rname rate payload <<<"$rs"; IFS='|' read -r cname bin envs extra <<<"$cs"
  local tag="$cname"; [ "$ck" = no ] && tag="G"
  local o="$OUT/$fs/$rname-$tag-r$round"; rm -rf "$o"; mkdir -p "$o"
  local d="$J/run"; rm -rf "$d"; sync; sleep 1
  local ckargs="--checkpoint-every 1s"; [ "$ck" = no ] && ckargs=""
  local cmd="env $envs $BIN/tt-sustained-$bin run --dir $d --out $o/out --rate $rate --payload $payload --duration $DUR --warmup $WARM $ckargs --readers mapped --cpus $W,$R --helper-cpus $H --json $extra"
  echo "$cmd" > "$o/command.txt"
  local bpid=""
  if [ "$traced" = yes ]; then
    sudo taskset -c 0 bpftrace "$HERE/ckpt.bt" "$KDEV" > "$o/trace.txt" 2>&1 &
    bpid=$!
    for _ in $(seq 1 60); do grep -qE "Attach(ing|ed)" "$o/trace.txt" 2>/dev/null && break; sleep 0.5; done
    sleep 1
    if ! ps -p "$bpid" >/dev/null || ! grep -qE "Attach(ing|ed)" "$o/trace.txt"; then echo "bpftrace failed: $o"; touch "$o/TRACE_FAILED"; fi
  fi
  cat "$STAT" > "$o/diskstat.before"; date +%s.%N > "$o/t.before"
  $cmd > "$o/result.json" 2> "$o/stderr.txt"; local rc=$?
  date +%s.%N > "$o/t.after"; cat "$STAT" > "$o/diskstat.after"
  if [ -n "$bpid" ]; then sudo kill -INT "$bpid"; wait "$bpid"; fi
  echo "$(date -u +%FT%TZ) $fs $rname $tag r$round traced=$traced rc=$rc"
  rm -rf "$d"
}

for fs in $FSLIST; do
  if [ "${LOCAL:-0}" = 1 ]; then J="$JROOT/$fs"; mkdir -p "$J"
  else
    J=/mnt/j; sudo umount /mnt/j 2>/dev/null; mountpoint -q /data && sudo umount /data
    if [ "$fs" = ext4 ]; then sudo mkfs.ext4 -q -F -E lazy_itable_init=0,lazy_journal_init=0 "$DEVPATH"
    else sudo mkfs.xfs -q -f "$DEVPATH"; fi
    sudo mkdir -p /mnt/j && sudo mount -o noatime "$DEVPATH" /mnt/j && sudo chown "$USER:$USER" /mnt/j
  fi
  { mount | grep " $J\| /mnt/j"; [ "$fs" = ext4 ] && sudo dumpe2fs -h "$DEVPATH" 2>/dev/null | grep -iE "features|journal size"; [ "$fs" = xfs ] && xfs_info "$J"; } > "$OUT/fs-$fs.txt" 2>&1
  for round in $(seq "${FIRST_ROUND:-1}" "$ROUNDS"); do
    traced=no; [ "$round" = "$ROUNDS" ] && traced=yes
    n=${#RATES[@]}
    for i in $(seq 0 $((n - 1))); do
      rs=${RATES[$(( (i + round) % n ))]}
      m=${#CONFIGS[@]}
      for j in $(seq 0 $((m - 1))); do
        run_one "$fs" "$round" "$rs" "${CONFIGS[$(( (j + i + round) % m ))]}" "$traced" yes
      done
      case "$rs" in r200k*) run_one "$fs" "$round" "$rs" "${CONFIGS[0]}" "$traced" no ;; esac
    done
  done
done
echo "$(date -u +%FT%TZ) done"
(cd "$(dirname "$OUT")" && tar czf "$HOME/ckpt.tgz" --exclude="*.series.jsonl" "$(basename "$OUT")") && sha256sum "$HOME/ckpt.tgz" > "$HOME/ckpt.tgz.sha256"
touch "$OUT/${DONE_FILE:-DONE}"
