#!/usr/bin/env bash
# Runs on the benchmark VM: drives benches/rerun.py through the untuned parts, reboots with isolated CPUs for the tuned
# parts, and leaves RESULTS/DONE at the end whatever happened, so that the watcher copies the results and deletes the VM.
# Usage: rerun-vm.sh [run|tune|untune|status]   Settings: $HOME/rerun.conf (see docs/playbooks/rerun-comparison.md):
#   PARTS="--part short:/data/tt ..."      untuned parts, run first
#   TUNED_PARTS="--part tuned-short:..."   parts for the isolated boot; empty: no reboot
#   ISOLATED="1,2"                         writer and reader CPU of the isolated boot, on different physical cores
#   DEADLINE="2026-11-27T10:00:00+00:00"   no unit may run past it (some minutes before the VM deletes itself)
#   EXTRA="--quick --units ..."            further options of rerun.py
#   POWEROFF_AFTER_MIN=60                  stop the VM if nobody deleted it after DONE; the boot disk keeps the results
set -uo pipefail
SELF="$(readlink -f "$0")"; REPO="$(dirname "$(dirname "$SELF")")"
RESULTS="${RESULTS:-$HOME/results}"; CONF="$HOME/rerun.conf"
PARTS=""; TUNED_PARTS=""; ISOLATED="1,2"; DEADLINE=""; EXTRA=""; POWEROFF_AFTER_MIN=60
# shellcheck disable=SC1090
[ -f "$CONF" ] && . "$CONF"
GRUB=/etc/default/grub.d/99-tickertape-rerun.cfg
mkdir -p "$RESULTS"
note() { echo "$(date -u +%FT%TZ) $*" | tee -a "$RESULTS/progress.txt"; }

mount_disks() {  # Same order as provision.sh; a guest reboot keeps the data but not the mounts.
  local devs; mapfile -t devs < <(ls /dev/disk/by-id/google-local-nvme-ssd-* 2>/dev/null | grep -v part)
  for i in 0 1; do
    local dir=/data; [ "$i" = 1 ] && dir=/data2
    if [ -n "${devs[$i]:-}" ] && ! mountpoint -q "$dir"; then sudo mkdir -p "$dir"; sudo mount -o noatime "${devs[$i]}" "$dir"; fi
  done
}

cmdline() {  # Kernel options of the isolated boot. nosmt leaves one thread per core online.
  local online rest
  online=$(lscpu -p=CPU,CORE | grep -v '^#' | awk -F, '!seen[$2]++ {print $1}')
  rest=$(comm -23 <(sort <<<"$online") <(tr , '\n' <<<"$ISOLATED" | sort) | sort -n | paste -sd, -)
  echo "isolcpus=managed_irq,domain,$ISOLATED nohz_full=$ISOLATED rcu_nocbs=$ISOLATED irqaffinity=$rest nosmt"
}

tune() {
  echo "GRUB_CMDLINE_LINUX=\"\$GRUB_CMDLINE_LINUX $(cmdline)\"" | sudo tee "$GRUB" > /dev/null
  sudo update-grub > "$RESULTS/update-grub.log" 2>&1 || { note "update-grub failed"; return 1; }
  # cron starts this copy outside /data, which is not mounted after a boot; it mounts /data and hands over.
  cat > "$HOME/rerun-boot.sh" <<EOF
#!/usr/bin/env bash
mountpoint -q /data || { sudo mkdir -p /data; sudo mount -o noatime \$(ls /dev/disk/by-id/google-local-nvme-ssd-* | grep -v part | head -1) /data; }
export RESULTS=$RESULTS
exec $SELF run
EOF
  chmod +x "$HOME/rerun-boot.sh"
  (crontab -l 2>/dev/null | grep -v rerun-boot.sh; echo "@reboot sleep 20; $HOME/rerun-boot.sh >> $RESULTS/vm.log 2>&1") | crontab -
}

untune() { sudo rm -f "$GRUB"; sudo update-grub > /dev/null 2>&1; crontab -l 2>/dev/null | grep -v rerun-boot.sh | crontab -; }

isolated_now() { cat /sys/devices/system/cpu/isolated; }

expand_list() {  # 1-3,8 -> one CPU per line
  local part; for part in ${1//,/ }; do seq "${part%-*}" "${part#*-}"; done
}

tuned_options() {
  local w r helpers; IFS=, read -r w r <<<"$ISOLATED"
  helpers=$(lscpu -p=CPU | grep -v '^#' | grep -vxE "$w|$r" | paste -sd, -)
  echo "--writer-cpu $w --reader-cpu $r --helper-cpus $helpers"
}

finish() {
  note "finished: $1"
  crontab -l 2>/dev/null | grep -v rerun-boot.sh | crontab - 2>/dev/null
  echo "$1" > "$RESULTS/DONE"
  if [ "$POWEROFF_AFTER_MIN" -gt 0 ]; then sudo shutdown -h "+$POWEROFF_AFTER_MIN" > /dev/null 2>&1; fi
  exit 0
}

driver() {  # driver <options...>
  local deadline=(); [ -n "$DEADLINE" ] && deadline=(--deadline "$DEADLINE")
  # An interrupted driver (exit 130) resumes, at most twice, so a signal does not start the next stage early.
  for _ in 1 2 3; do
    # shellcheck disable=SC2086
    python3 "$REPO/benches/rerun.py" --out "$RESULTS/rerun" "${deadline[@]}" $EXTRA "$@" >> "$RESULTS/driver.log" 2>&1
    rc=$?; [ $rc -ne 130 ] && break
    note "the driver was interrupted; resuming"
  done
  return $rc
}

case "${1:-run}" in
  status) tail -n 5 "$RESULTS/progress.txt"; tail -n 5 "$RESULTS/driver.log"; isolated_now; exit 0 ;;
  tune) tune; exit $? ;;
  untune) untune; exit 0 ;;
  run) ;;
  *) echo "usage: rerun-vm.sh [run|tune|untune|status]" >&2; exit 2 ;;
esac

# Copying results to the watcher must not run on a measured CPU: every ssh session (tar, encryption) gets CPU 0, which
# no pinned configuration measures on (unpinned ones can); the drop-in keeps it so after the reboot. This script may itself come from such a
# session, so it takes the other CPUs back first (all but isolated ones, as a process started at boot has them).
sudo mkdir -p /etc/systemd/system/ssh.service.d
printf '[Service]\nCPUAffinity=0\n' | sudo tee /etc/systemd/system/ssh.service.d/rerun-cpu.conf > /dev/null
sudo systemctl daemon-reload
for pid in $(pgrep -x sshd); do sudo taskset -cp 0 "$pid" > /dev/null 2>&1; done
taskset -acp "$(comm -23 <(lscpu -p=CPU | grep -v '^#' | sort) <(expand_list "$(isolated_now)" | sort) | sort -n | paste -sd, -)" $$ > /dev/null
exec 9> "$RESULTS/vm.lock"; flock -n 9 || { echo "already running"; exit 1; }
[ -f "$RESULTS/DONE" ] && exit 0
mount_disks
# shellcheck disable=SC1091
. "$HOME/.cargo/env"
JAVA_HOME=$(dirname "$(dirname "$(readlink -f "$(command -v javac)")")"); export JAVA_HOME
# Package updates, snap refreshes and TRIM must not start inside a measurement; what stays on is in the environment file.
for unit in unattended-upgrades.service apt-daily.timer apt-daily-upgrade.timer man-db.timer fstrim.timer \
            motd-news.timer fwupd-refresh.timer update-notifier-download.timer snapd.service snapd.socket \
            google-osconfig-agent.service; do sudo systemctl stop "$unit" > /dev/null 2>&1; done
for dir in /data /data2; do mountpoint -q "$dir" && mkdir -p "$dir/tt"; done
note "sshd CPUs: $(for pid in $(pgrep -x sshd | head -1); do taskset -cp "$pid"; done); driver CPUs: $(taskset -cp $$)"
note "boot: isolated='$(isolated_now)' cmdline: $(cat /proc/cmdline)"
failed=""
if [ -z "$(isolated_now)" ]; then
  if [ -n "$PARTS" ]; then
    note "start untuned parts: $PARTS"
    # shellcheck disable=SC2086
    driver $PARTS; rc=$?; note "end untuned parts exit=$rc"; [ $rc -ne 0 ] && failed="untuned exit=$rc"
  fi
  if [ -n "$TUNED_PARTS" ]; then
    [ -f "$RESULTS/tuned-boot-requested" ] && finish "the isolated boot did not isolate any CPU $failed"
    tune || finish "tune failed $failed"
    touch "$RESULTS/tuned-boot-requested"; echo "$failed" > "$RESULTS/failed-so-far"
    note "reboot with: $(cmdline)"; sync; sudo systemctl reboot; sleep 120; finish "reboot did not happen"
  fi
else
  failed=$(cat "$RESULTS/failed-so-far" 2>/dev/null)
  note "start tuned parts: $TUNED_PARTS on $(tuned_options)"
  # shellcheck disable=SC2086
  driver --tuned-options "$(tuned_options)" $TUNED_PARTS; rc=$?; note "end tuned parts exit=$rc"
  [ $rc -ne 0 ] && failed="$failed tuned exit=$rc"
fi
finish "${failed:-ok}"
