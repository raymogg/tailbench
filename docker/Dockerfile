# TODO(before step 4): replace both tags with digests.
#
# Digest pinning is required -- a moving tag changes libc and kernel-visible
# behaviour between admission runs and the eval that cites them, invalidating
# the numbers silently. Tags are used here only so this builds today; resolve
# them on the Linux host with:
#   docker pull rust:1.98-bookworm && docker inspect --format='{{index .RepoDigests 0}}' rust:1.98-bookworm
# This MUST be done before any admission run.
FROM rust:1.98-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --bins

FROM debian:bookworm-slim
COPY --from=build /src/target/release/loadgen /usr/local/bin/loadgen
COPY --from=build /src/target/release/mocks /usr/local/bin/mocks
RUN mkdir -p /run/tailbench /out
