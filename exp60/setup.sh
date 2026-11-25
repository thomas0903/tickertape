#!/usr/bin/env bash
# On the VM, after provisioning (ext4 on /data, repository in /data/tickertape): tracer and every build, before any run.
# bpftrace: Ubuntu 24.04's 0.20 cannot parse the diagnostics scripts; the release AppImage needs extracting (no FUSE).
set -e
A=$(uname -m)
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq jq >/dev/null 2>&1
sudo mkdir -p /opt/bt && cd /opt/bt
sudo curl -sSL -o bpftrace.appimage https://github.com/bpftrace/bpftrace/releases/download/v0.27.1/bpftrace-$A
sudo chmod +x bpftrace.appimage && sudo ./bpftrace.appimage --appimage-extract >/dev/null && sudo ln -sf /opt/bt/squashfs-root/AppRun bpftrace
sudo ./bpftrace --version
. $HOME/.cargo/env
cd /data/tickertape && git fetch -q origin && git fetch -q origin refs/pull/28/head
for v in v1.0.0:main100 origin/exp/roll-60:exp origin/exp/roll-60:exp2 f194c429:finish origin/t3/investigate-roll-latency:draft; do
  ref=${v%%:*}; n=${v##*:}; git checkout -q $ref
  CARGO_TARGET_DIR=$HOME/t cargo build -q --release -p tickertape-bench --bin tt-sustained --bin tt-latency 2>/dev/null
  mkdir -p ~/b/$n; cp ~/t/release/tt-sustained ~/t/release/tt-latency ~/b/$n/; git rev-parse HEAD > ~/b/$n/commit
done
cp -r benches/diagnostics ~/diag; mkdir -p ~/r
rustc --version; head ~/b/*/commit
