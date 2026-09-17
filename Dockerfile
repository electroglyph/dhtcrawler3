# syntax=docker/dockerfile:1.7
#
# Reproducible, source-only build of dhtcrawler3 (requirement R13).
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
    cargo build --release --locked -p dhtcrawler3 \
 && install -D -m 0755 target/release/dhtcrawler3 /out/bin/dhtcrawler3 \
 && install -d -m 0750 /out/data /out/data/index

FROM ${RUNTIME_IMAGE}
LABEL org.opencontainers.image.title="dhtcrawler3" \
      org.opencontainers.image.description="A secure, standards-compliant BitTorrent DHT search engine" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.source="https://github.com/poonasor/dhtcrawler3"
COPY --from=build /out/bin/dhtcrawler3 /usr/local/bin/dhtcrawler3
COPY --from=build --chown=65532:65532 /out/data /var/lib/dhtcrawler3
COPY deploy/config/dhtcrawler3.toml /etc/dhtcrawler3/dhtcrawler3.toml
USER 65532:65532
VOLUME ["/var/lib/dhtcrawler3"]
ENTRYPOINT ["/usr/local/bin/dhtcrawler3", "--config", "/etc/dhtcrawler3/dhtcrawler3.toml"]
CMD ["all"]
