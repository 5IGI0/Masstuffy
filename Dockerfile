# syntax=docker/dockerfile:1

# ---- build -------------------------------------------------------------------------------------
# The workspace sets rust-version = 1.94; the zstd crate builds libzstd from C (gcc ships in the
# slim Rust image).
FROM docker.io/library/rust:1.94-slim-bookworm AS build
WORKDIR /src
COPY . .
# Cache mounts keep the cargo registry and the target dir across builds; the binary is copied out
# of the cache-mounted target dir so the next stage can see it.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bin masstuffy-server \
 && install -D target/release/masstuffy-server /out/masstuffy-server

# ---- runtime -----------------------------------------------------------------------------------
FROM docker.io/library/debian:bookworm-slim
# Fixed uid/gid 10001, so a host directory can be mapped to it (podman: --userns=keep-id:uid=10001,gid=10001).
RUN groupadd --system --gid 10001 masstuffy \
 && useradd --system --uid 10001 --gid 10001 --home-dir /data --no-create-home masstuffy \
 && install -d -o masstuffy -g masstuffy /data
COPY --from=build /out/masstuffy-server /usr/local/bin/masstuffy-server

USER masstuffy
VOLUME /data
EXPOSE 8470

# Inside a container the server must listen on all interfaces. It has no authentication: publish
# the port on localhost only, or put an authenticating reverse proxy in front.
ENV MASSTUFFY_WORKDIR=/data \
    MASSTUFFY_LISTEN=0.0.0.0:8470 \
    MASSTUFFY_INIT=1

# The server handles SIGTERM itself (first signal drains, second stops), so it runs as PID 1.
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/masstuffy-server"]
