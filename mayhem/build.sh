#!/usr/bin/env bash
#
# mayhem/build.sh — build risingwave_sqlparser cargo-fuzz target(s) as sanitized libFuzzer
# binaries, AND build the sqlparser crate's clean test binaries for mayhem/test.sh.
set -euo pipefail

[ -n "${SOURCE_DATE_EPOCH:-}" ] || unset SOURCE_DATE_EPOCH

: "${SRC:=/mayhem}"
# ASan fuzz + workspace test compile is RAM-heavy; default low parallelism (override via MAYHEM_JOBS).
: "${MAYHEM_JOBS:=2}"
export CARGO_BUILD_JOBS="$MAYHEM_JOBS"

cd "$SRC"

: "${RUST_DEBUG_FLAGS:=-C debuginfo=2 -C force-frame-pointers=yes}"
DWARF_FLAGS="-Zdwarf-version=3"
# --cfg madsim skips the workspace-hack dependency on risingwave_sqlparser so the
# detached mayhem/fuzz crate does not pull the entire workspace graph.
FUZZ_RUSTFLAGS="${RUSTFLAGS:-} --cfg fuzzing --cfg madsim -Zsanitizer=address ${RUST_DEBUG_FLAGS} ${DWARF_FLAGS}"
echo "SANITIZER_FLAGS (base, informational) = ${SANITIZER_FLAGS:-<unset>}"

FUZZ_DIR="mayhem/fuzz"
TRIPLE="x86_64-unknown-linux-gnu"

export CFLAGS="${CFLAGS:-} -gdwarf-3"
export CXXFLAGS="${CXXFLAGS:-} -gdwarf-3"

ASAN_A="$(rustc --print sysroot)/lib/rustlib/${TRIPLE}/lib/librustc-nightly_rt.asan.a"
if [ -f "$ASAN_A" ]; then
  echo "stripping debug info from prebuilt ASan runtime: $ASAN_A"
  objcopy --strip-debug "$ASAN_A" 2>/dev/null || objcopy --remove-section '.debug_*' "$ASAN_A" 2>/dev/null || true
fi

FUZZ_TARGETS=()
for f in "$FUZZ_DIR"/fuzz_targets/*.rs; do
  FUZZ_TARGETS+=("$(basename "${f%.*}")")
done
[ "${#FUZZ_TARGETS[@]}" -gt 0 ] || { echo "ERROR: no fuzz targets under $FUZZ_DIR/fuzz_targets/" >&2; exit 1; }

echo "=== cargo fuzz build (ASan via RUSTFLAGS, DWARF 3) ==="
echo "RUSTFLAGS=$FUZZ_RUSTFLAGS"
echo "targets: ${FUZZ_TARGETS[*]}"

for t in "${FUZZ_TARGETS[@]}"; do
  echo "--- building fuzz target: $t ---"
  RUSTFLAGS="$FUZZ_RUSTFLAGS" cargo fuzz build --fuzz-dir "$FUZZ_DIR" -O --debug-assertions "$t"
  bin="$SRC/$FUZZ_DIR/target/$TRIPLE/release/$t"
  [ -x "$bin" ] || { echo "ERROR: expected fuzz binary not found at $bin" >&2; exit 1; }
  cp "$bin" "/mayhem/$t"
  echo "built /mayhem/$t"
done

echo "=== cargo test --no-run -p risingwave_sqlparser (clean flags, for test.sh) ==="
# --cfg madsim skips workspace-hack so we only compile sqlparser + deps, not the full workspace graph.
TEST_RUSTFLAGS="--cap-lints=warn --cfg madsim"
( cd "$SRC" && env -u RUSTFLAGS CARGO_TARGET_DIR="$SRC/mayhem/test-target" \
  RUSTFLAGS="$TEST_RUSTFLAGS" cargo test --no-run -p risingwave_sqlparser )
echo "test binaries built"

echo "build.sh complete"
