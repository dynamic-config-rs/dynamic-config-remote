# Build stage: the workspace, release profile, just the server binary.
FROM rust:1.97-slim@sha256:8e8cf8f7fd54a2d23d5a743b3a03f56e26b6c774276c33fa0595111704ebb15c AS build
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
