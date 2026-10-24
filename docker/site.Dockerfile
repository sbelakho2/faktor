# Faktor website image: Zola static build + Axum service.
# Build from the repository root:
#   docker build -f docker/site.Dockerfile -t faktor-site .
FROM ghcr.io/getzola/zola:v0.22.1@sha256:b491a8bec3773815bdcdbbfbc2d0d60eda880ef550c4cc512349ab6451fbd72a AS zola
FROM rust:1.98.0@sha256:620dbcd124499c59e2406d3741574b5c5838cf9eb9656f0c3a03948f79b02959 AS sitebuild
COPY --from=zola /bin/zola /usr/local/bin/zola
ARG SITE_BASE_URL=http://127.0.0.1:8080
WORKDIR /src
COPY site ./site
RUN zola --root site build --base-url "${SITE_BASE_URL}"

FROM rust:1.98.0@sha256:620dbcd124499c59e2406d3741574b5c5838cf9eb9656f0c3a03948f79b02959 AS serverbuild
WORKDIR /src
COPY . .
RUN cargo build --release -p faktor-site

FROM debian:bookworm-slim@sha256:7ff6ba31527b9e336162fa061746c366c4b2032f4d2cff2078bea43f78efe3f8
ARG SITE_BASE_URL=http://127.0.0.1:8080
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=serverbuild /src/target/release/faktor-site /usr/local/bin/faktor-site
COPY --from=sitebuild /src/site/public /app/site
ENV FAKTOR_SITE_DATA_DIR=/data \
    FAKTOR_SITE_STATIC_DIR=/app/site \
    FAKTOR_SITE_BIND=0.0.0.0:8080 \
    FAKTOR_SITE_BASE_URL="${SITE_BASE_URL}" \
    FAKTOR_SITE_BILLING_PROVIDER=local
VOLUME ["/data"]
EXPOSE 8080
ENTRYPOINT ["faktor-site", "serve"]
