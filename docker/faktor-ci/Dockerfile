# syntax=docker/dockerfile:1
# Faktor CI image (P2-D): the dedicated apt toolchain for the linux lanes.
#
# Every input is pinned:
#   * base image by multi-arch index digest (ubuntu:24.04 @ 2026-09-23);
#   * apt by the fixed Ubuntu noble snapshot 20260923T000000Z
#     (snapshot.ubuntu.com, release + updates + security pockets);
#   * every package by an exact `pkg=version` pin, re-verified after the
#     install with dpkg-query (the build fails on version drift).
#
# Once `scripts/build-ci-image.sh` pushes this image, the lanes switch to
# `image: <ref>@sha256:<digest>` and their apt steps disappear (annotate with
# `# apt-pinned: <ref@sha256:...>`). Until then the workflows run the same
# snapshot+exact-version pinning inline with `# apt-snapshot:` annotations —
# no job touches live distribution repositories either way.
#
# Trust bootstrapping: ubuntu:24.04 ships without ca-certificates, so the
# single snapshot index fetch for the pinned `ca-certificates` install has
# TLS peer/host verification disabled. APT's InRelease signature remains the
# trust anchor there (ubuntu-keyring ships in the base image); the bypass is
# removed before anything else is fetched, and every later fetch verifies TLS
# again.
FROM ubuntu:24.04@sha256:008173c23f95b170204355c12626cb5a965d779a7e1283b09e9cffbb1bf33ca3

ARG SNAPSHOT=20260923T000000Z
ENV DEBIAN_FRONTEND=noninteractive

RUN set -eux; \
    printf 'deb [check-valid-until=no] https://snapshot.ubuntu.com/ubuntu/%s noble main universe\n' "$SNAPSHOT" > /etc/apt/sources.list.d/faktor-snapshot.list; \
    printf 'deb [check-valid-until=no] https://snapshot.ubuntu.com/ubuntu/%s noble-updates main universe\n' "$SNAPSHOT" >> /etc/apt/sources.list.d/faktor-snapshot.list; \
    printf 'deb [check-valid-until=no] https://snapshot.ubuntu.com/ubuntu/%s noble-security main universe\n' "$SNAPSHOT" >> /etc/apt/sources.list.d/faktor-snapshot.list; \
    rm -f /etc/apt/sources.list /etc/apt/sources.list.d/ubuntu.sources; \
    printf 'Acquire::https::Verify-Peer "false";\nAcquire::https::Verify-Host "false";\n' > /etc/apt/apt.conf.d/99faktor-ca-bootstrap; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates=20260601~24.04.1; \
    rm -f /etc/apt/apt.conf.d/99faktor-ca-bootstrap; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
        build-essential=12.10ubuntu1 \
        curl=8.5.0-2ubuntu10.13 \
        pkg-config=1.8.1-2build1 \
        libssl-dev=3.0.13-0ubuntu3.15 \
        kotlin=1.3.31+ds1-1ubuntu1 \
        openjdk-11-jre-headless=11.0.32.1+1-1ubuntu1~24.04 \
        unzip=6.0-28ubuntu4.1 \
        python3=3.12.3-0ubuntu2.1 \
        procps=2:4.0.4-4ubuntu3.3; \
    for spec in \
        ca-certificates=20260601~24.04.1 \
        build-essential=12.10ubuntu1 \
        curl=8.5.0-2ubuntu10.13 \
        pkg-config=1.8.1-2build1 \
        libssl-dev=3.0.13-0ubuntu3.15 \
        kotlin=1.3.31+ds1-1ubuntu1 \
        openjdk-11-jre-headless=11.0.32.1+1-1ubuntu1~24.04 \
        unzip=6.0-28ubuntu4.1 \
        python3=3.12.3-0ubuntu2.1 \
        procps=2:4.0.4-4ubuntu3.3; do \
        pkg="${spec%%=*}"; want="${spec#*=}"; got="$(dpkg-query -W -f='${Version}' "$pkg")"; \
        [ "$got" = "$want" ] || { echo "version drift: $pkg = $got (want $want)" >&2; exit 1; }; \
    done; \
    rm -rf /var/lib/apt/lists/*
