# syntax=docker/dockerfile:1.7
#
# Commit-image Dockerfile — RUST target (cargo / cargo-fuzz) for risingwave.

FROM ghcr.io/mayhemheroes/base:20260621

ARG COMMIT
ARG REPO_URL
ARG SOURCE_DATE_EPOCH
ARG SANITIZER_FLAGS

# Match upstream ci/rust-toolchain (nightly-2026-06-11).
ARG RUST_CHANNEL=nightly-2026-06-11
ARG CARGO_FUZZ_VERSION=0.12.0
ARG RUST_STABLE=1.84.0

ENV RUSTUP_HOME=/opt/toolchains/rust/rustup \
    CARGO_HOME=/opt/toolchains/rust/cargo \
    PATH=/opt/toolchains/rust/cargo/bin:${PATH}

USER root
RUN install -d /opt/toolchains/rust \
 && curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs -o /tmp/rustup-init.sh \
 && sh /tmp/rustup-init.sh -y --no-modify-path --profile minimal \
        --default-toolchain "${RUST_CHANNEL}" \
 && rustup component add rust-src --toolchain "${RUST_CHANNEL}" \
 && rustup toolchain install "${RUST_STABLE}" --profile minimal \
 && cargo "+${RUST_STABLE}" install -j1 cargo-fuzz --version "${CARGO_FUZZ_VERSION}" --locked \
 && rm -f /tmp/rustup-init.sh \
 && chown -R 2000:2000 /opt/toolchains/rust

USER mayhem
WORKDIR /mayhem
# BuildKit uses mayhem/Dockerfile.dockerignore (not upstream's .git/ exclusion).
COPY --chown=2000:2000 .git /mayhem/.git
COPY --chown=2000:2000 . /mayhem

RUN bash mayhem/build.sh

RUN if [ -f mayhem/test.sh ]; then bash mayhem/test.sh; else echo "no mayhem/test.sh"; fi

LABEL org.opencontainers.image.source="${REPO_URL}" \
      org.opencontainers.image.revision="${COMMIT}" \
      rlenv.tier="commit" \
      rlenv.source_date_epoch="${SOURCE_DATE_EPOCH}" \
      rlenv.user="mayhem"

USER mayhem
WORKDIR /mayhem
