#!/bin/bash
set -euo pipefail

# RLENV Build Script
# This script rebuilds the application from source located at /rlenv/source/risingwave/
#
# Original image: ghcr.io/mayhemheroes/risingwave:main
# Git revision: 0e8532d12f11b22c5e6105cf276703df23d83ed7

# Set up Rust environment (installed in globally accessible location)
export RUSTUP_HOME=/opt/rust/rustup
export CARGO_HOME=/opt/rust/cargo
export PATH="/opt/rust/cargo/bin:${PATH}"

# Change to the source directory
cd /rlenv/source/risingwave

# Build the fuzz target using honggfuzz
cd src/sqlparser/fuzz
RUSTFLAGS="-Cpasses=sancov-module -Clink-arg=-fuse-ld=lld" \
cargo +nightly hfuzz build

# Ensure output directories exist and are world-writable
mkdir -p /out
chmod 777 /out 2>/dev/null || true

# Copy the compiled fuzz binary to expected locations
# Use -f flag to force overwrite without unlinking (works for unprivileged users)
cp -f hfuzz_target/x86_64-unknown-linux-gnu/release/fuzz_parse_sql /out/
cp -f hfuzz_target/x86_64-unknown-linux-gnu/release/fuzz_parse_sql /

# Make the output files world-writable for unprivileged users
chmod 777 /out/fuzz_parse_sql 2>/dev/null || true
chmod 777 /fuzz_parse_sql 2>/dev/null || true

# Verify build artifacts exist
if [ ! -f /fuzz_parse_sql ]; then
    echo "Error: Build artifact not found at /fuzz_parse_sql"
    exit 1
fi

if [ ! -f /out/fuzz_parse_sql ]; then
    echo "Error: Build artifact not found at /out/fuzz_parse_sql"
    exit 1
fi

echo "Build completed successfully!"
echo "Fuzz binary available at:"
echo "  /fuzz_parse_sql"
echo "  /out/fuzz_parse_sql"
