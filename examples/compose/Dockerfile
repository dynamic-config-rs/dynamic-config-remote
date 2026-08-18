# Build stage: the workspace, release profile, just the server binary.
FROM rust:1.88-slim AS build
WORKDIR /src
COPY . .
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler pkg-config libssl-dev \
 && cargo build --release -p dynamic-config-server

# Run stage: distroless-ish slim, one binary, no shell needed at runtime
# (the README's `exec sh` is for the demo edit; drop to distroless in
# production and edit the mounted file from the host instead).
FROM debian:bookworm-slim
COPY --from=build /src/target/release/dynamic-config-server /usr/local/bin/
EXPOSE 8155
ENTRYPOINT ["dynamic-config-server"]
