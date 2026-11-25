#!/usr/bin/env bash
# Format-v1 compatibility fixtures (tests/compat/README.md).
#
#   tests/compat/generate.sh TAG...                 write fixtures with each tag's own writer, add them to
#                                                   tests/compat/fixtures/ and manifest.json (this OS and CPU)
#   tests/compat/generate.sh --check-old-readers [TAG...]
#                                                   write journals with the working tree's writer and check that
#                                                   each tag's readers and verify see exactly what it wrote
#                                                   (default: every v* tag)
#
# Each tag's library is built from `git archive` of the tagged commit in a temporary directory (outside the
# repository; nothing in the checkout or .git changes except the fixture files), with the tag's own Cargo.lock.
# Journals are written under $TMPDIR, which must be a local filesystem (see docs/playbooks/test-on-linux.md).
# Needs git, cargo and python3.
set -euo pipefail

repo=$(git rev-parse --show-toplevel)
fixtures="$repo/tests/compat/fixtures"
work=$(mktemp -d "${TMPDIR:-/tmp}/tt-compat.XXXXXX")
trap 'rm -rf "$work"' EXIT

case "$(uname -s)" in
  Darwin) os=macos ;;
  Linux) os=linux ;;
  *) echo "unsupported OS $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  arm64 | aarch64) arch=aarch64 ;;
  x86_64 | amd64) arch=x86_64 ;;
  *) echo "unsupported CPU $(uname -m)" >&2; exit 1 ;;
esac

# extract LABEL REV: the tree of REV in $work/src-LABEL (prints the directory).
extract() {
  local src="$work/src-$1"
  mkdir -p "$src"
  git -C "$repo" archive --format=tar "$2" | tar -x -C "$src"
  echo "$src"
}

# build LABEL SRC: builds the generator against SRC/crates/tickertape (prints the binary).
build() {
  local label=$1 src=$2 gen="$work/gen-$1" features=""
  # ReaderConfig::durable_only exists since v0.2.0.
  if grep -q "pub fn durable_only" "$src/crates/tickertape/src/config.rs"; then
    features="durable-only"
  fi
  mkdir -p "$gen"
  cat >"$gen/Cargo.toml" <<EOF
[package]
name = "tt-compat-gen"
version = "0.0.0"
edition = "2021"
publish = false

[workspace]

[[bin]]
name = "tt-compat-gen"
path = "$repo/tests/compat/generator/main.rs"

[dependencies]
tickertape = { path = "$src/crates/tickertape" }

[features]
durable-only = []
EOF
  # The dependency versions that release was built and tested with.
  cp "$src/Cargo.lock" "$gen/Cargo.lock"
  CARGO_TARGET_DIR="$work/target-$label" cargo build --quiet --release \
    --manifest-path "$gen/Cargo.toml" ${features:+--features "$features"} >&2
  echo "$work/target-$label/release/tt-compat-gen"
}

generate() {
  mkdir -p "$fixtures"
  local tag commit src bin kind name platform rustc_version
  platform="$(uname -srm)"
  if [ "$os" = macos ]; then platform="macOS $(sw_vers -productVersion) ($platform)"; fi
  # Describe the machine and filesystem, e.g. TT_COMPAT_PLATFORM_NOTE="OrbStack VM (Linux on macOS); journals on btrfs".
  platform="$platform${TT_COMPAT_PLATFORM_NOTE:+; $TT_COMPAT_PLATFORM_NOTE}"
  rustc_version=$(rustc -V)
  for tag in "$@"; do
    commit=$(git -C "$repo" rev-parse --verify "$tag^{commit}")
    src=$(extract "$tag" "$commit")
    bin=$(build "$tag" "$src")
    for kind in clean torn; do
      name="$tag-$os-$arch-$kind"
      "$bin" "$kind" "$work/out/$name" >"$work/$name.json"
      python3 -I "$repo/tests/compat/pack.py" pack "$work/out/$name" "$fixtures/$name.ttfx"
      python3 -I "$repo/tests/compat/pack.py" add "$fixtures/manifest.json" "$work/$name.json" \
        --name "$name" --pack "$fixtures/$name.ttfx" --tag "$tag" --commit "$commit" --os "$os" \
        --arch "$arch" --platform "$platform" --rustc "$rustc_version" \
        --command "tests/compat/generate.sh $tag, which runs: tt-compat-gen $kind <dir>"
      echo "$name: written by $tag ($commit) on $os $arch" >&2
    done
  done
}

check_old_readers() {
  local tags=("$@") bin_new bin kind tag failed=0
  if [ ${#tags[@]} -eq 0 ]; then
    while IFS= read -r tag; do tags+=("$tag"); done < <(git -C "$repo" tag --list 'v*' --sort=version:refname)
  fi
  bin_new=$(build working-tree "$repo")
  for kind in clean torn; do
    "$bin_new" "$kind" "$work/new-$kind" >"$work/new-$kind.manifest.json"
    "$bin_new" read "$work/new-$kind" >"$work/new-$kind.read.json"
  done
  for tag in "${tags[@]}"; do
    bin=$(build "$tag" "$(extract "$tag" "$(git -C "$repo" rev-parse --verify "$tag^{commit}")")")
    for kind in clean torn; do
      # A copy, so an old version can't change the journal the next one reads.
      rm -rf "$work/copy" && cp -R "$work/new-$kind" "$work/copy"
      # With files in seek-index/ (ADR 0045), which every version must ignore. (Segments this small get no index.)
      mkdir "$work/copy/seek-index"
      for segment in "$work/copy"/*.tape; do
        cp "$segment" "$work/copy/seek-index/$(basename "$segment" .tape).idx"
      done
      "$bin" read "$work/copy" >"$work/old-$tag-$kind.json"
      if ! python3 -I - "$work/new-$kind.read.json" "$work/old-$tag-$kind.json" "$tag" "$kind" <<'EOF'; then failed=1; fi
import json, sys
new, old = (json.load(open(p)) for p in sys.argv[1:3])
tag, kind = sys.argv[3:5]
diffs = [k for k in ("safe", "mapped", "verify", "checkpoint_next_seq") if new[k] != old[k]]
if "durable" in old and old["durable"] != new["durable"]:
    diffs.append("durable")
status = "ok" if not diffs else "MISMATCH in " + ", ".join(diffs)
print(f"{tag} ({old['library_version']}) reading a {kind} journal written by the working tree ({new['library_version']}): "
      f"{old['safe']['records']} records, durable-only {'checked' if 'durable' in old else 'n/a'}: {status}")
if diffs:
    print(json.dumps({"expected": new, "got": old}, indent=2))
    sys.exit(1)
EOF
    done
  done
  return $failed
}

if [ $# -eq 0 ]; then
  sed -n '2,14p' "$0" >&2
  exit 1
fi
if [ "$1" = "--check-old-readers" ]; then
  shift
  check_old_readers "$@"
else
  generate "$@"
fi
