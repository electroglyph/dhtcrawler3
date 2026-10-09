# syntax=docker/dockerfile:1.7
#
# Reproducible, source-only build of dhtcrawler4 (requirement R13).
# Base images are pinned by multi-arch index digest; update them deliberately
# (Dependabot opens PRs for this file).

ARG RUST_IMAGE=rust:1.98-slim-trixie@sha256:bce1476d4be4d78b83705bc5f428b86d640eeeea33e9dadafbc037b5703a53bf
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian13:nonroot@sha256:54df941ed0d06a1bd95ef5e0ce391fd8d9f94b64782dc9a60062727849ee3f97

FROM ${RUST_IMAGE} AS build
WORKDIR /src
ENV CARGO_TERM_COLOR=never \
    CARGO_INCREMENTAL=0 \
    RUSTFLAGS="--remap-path-prefix=/src=. --remap-path-prefix=/usr/local/cargo=cargo"
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p dhtcrawler4 \
 && install -D -m 0755 target/release/dhtcrawler4 /out/bin/dhtcrawler4 \
 && install -d -m 0750 /out/data /out/data/index

FROM ${RUNTIME_IMAGE}
LABEL org.opencontainers.image.title="dhtcrawler4" \
      org.opencontainers.image.description="A secure, standards-compliant BitTorrent DHT search engine" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.source="https://github.com/electroglyph/dhtcrawler4"
COPY --from=build /out/bin/dhtcrawler4 /usr/local/bin/dhtcrawler4
COPY --from=build --chown=65532:65532 /out/data /var/lib/dhtcrawler4
COPY deploy/config/dhtcrawler4.toml /etc/dhtcrawler4/dhtcrawler4.toml
USER 65532:65532
VOLUME ["/var/lib/dhtcrawler4"]
ENTRYPOINT ["/usr/local/bin/dhtcrawler4", "--config", "/etc/dhtcrawler4/dhtcrawler4.toml"]
CMD ["all"]
