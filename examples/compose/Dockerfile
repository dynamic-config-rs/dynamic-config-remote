# Build stage: the workspace, release profile, just the server binary.
FROM rust:1.88-slim@sha256:38bc5a86d998772d4aec2348656ed21438d20fcdce2795b56ca434cf21430d89 AS build
WORKDIR /src
COPY . .
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler pkg-config libssl-dev \
 && cargo build --release -p dynamic-config-server

# Run stage: distroless-ish slim, one binary, no shell needed at runtime
# (the README's `exec sh` is for the demo edit; drop to distroless in
# production and edit the mounted file from the host instead).
FROM debian:bookworm-slim@sha256:abd67ffcfa541b485a3dff59865ab629aa048a6c613e639d36e7456b0b229241
COPY --from=build /src/target/release/dynamic-config-server /usr/local/bin/
EXPOSE 8155
ENTRYPOINT ["dynamic-config-server"]
