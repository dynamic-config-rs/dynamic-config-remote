# Build stage: the workspace, release profile, just the server binary.
FROM rust:1.98-slim@sha256:17d1ba895198f9934c6314ec5346a0d5115372f3243390c3d731e242f35c2f27 AS build
WORKDIR /src
COPY . .
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler pkg-config libssl-dev \
 && cargo build --release -p dynamic-config-server

# Run stage: distroless-ish slim, one binary, no shell needed at runtime
# (the README's `exec sh` is for the demo edit; drop to distroless in
# production and edit the mounted file from the host instead).
FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171
COPY --from=build /src/target/release/dynamic-config-server /usr/local/bin/
EXPOSE 8155
ENTRYPOINT ["dynamic-config-server"]
