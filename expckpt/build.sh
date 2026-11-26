#!/usr/bin/env bash
# Builds tt-sustained from main (the measured commit) and from exp/ckpt-prefault (switches, never merged) into ~/bin.
set -euo pipefail
MAIN_REF="$1"; EXP_REF="$2"; REPO="${REPO:-/data/tickertape}"
mkdir -p "$HOME/bin" "$HOME/ckpt"
source "$HOME/.cargo/env" 2>/dev/null || true
cd "$REPO"; git fetch -q origin "+refs/heads/exp/ckpt-prefault:refs/remotes/origin/exp/ckpt-prefault" || true
git checkout -q "$MAIN_REF"; cargo build -q --release --locked -p tickertape-bench --bin tt-sustained
cp target/release/tt-sustained "$HOME/bin/tt-sustained-main"
git checkout -q "$EXP_REF"; cargo build -q --release --locked -p tickertape-bench --bin tt-sustained
cp target/release/tt-sustained "$HOME/bin/tt-sustained-exp"
{ echo "main $(git rev-parse "$MAIN_REF")"; echo "exp $(git rev-parse "$EXP_REF")"; rustc --version; sha256sum "$HOME"/bin/tt-sustained-*; } | tee "$HOME/ckpt/binaries.txt"
