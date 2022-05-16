# Build Stage
FROM ubuntu:20.04 as builder

## Install build dependencies.
RUN apt-get update && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y cmake clang curl binutils-dev libunwind8-dev
RUN curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
RUN ${HOME}/.cargo/bin/rustup default nightly
RUN ${HOME}/.cargo/bin/cargo install honggfuzz --version "0.5.54"

## Add source code to the build stage.
ADD . /risingwave
WORKDIR /risingwave
RUN cd src/sqlparser/fuzz && \
	RUSTFLAGS="-Znew-llvm-pass-manager=no" HFUZZ_RUN_ARGS="--run_time $run_time --exit_upon_crash" ${HOME}/.cargo/bin/cargo +nightly hfuzz build

# Package Stage
FROM ubuntu:20.04

COPY --from=builder risingwave/src/sqlparser/fuzz/hfuzz_target/x86_64-unknown-linux-gnu/release/fuzz_parse_sql /