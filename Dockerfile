# syntax=docker/dockerfile:1
FROM rust:1.100-bookworm AS builder
# aws-lc-rs (via rustls) builds from source and needs a C toolchain and cmake on some targets.
RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake clang \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src

# Dependency layer: build against stub sources so it is cached until the manifests change.
COPY Cargo.toml Cargo.lock build.rs ./
RUN mkdir -p src migrations \
    && echo 'fn main() {}' > src/main.rs \
    && touch src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src target/release/deps/axl* target/release/.fingerprint/axl* target/release/axl*

COPY migrations ./migrations
COPY src ./src
COPY templates ./templates
COPY static ./static
# COPY keeps old mtimes; make cargo see the real sources as newer than the stubs.
RUN touch src/main.rs src/lib.rs \
    && cargo build --release --locked \
    && install -D target/release/axl /out/usr/local/bin/axl \
    && mkdir -p /out/data

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /out/usr/local/bin/axl /usr/local/bin/axl
COPY --from=builder --chown=65532:65532 /out/data /data
VOLUME /data
EXPOSE 7420
USER nonroot
ENTRYPOINT ["axl"]
CMD ["serve", "--bind", "0.0.0.0:7420"]
