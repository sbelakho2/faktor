# syntax=docker/dockerfile:1
# Faktor CI image (P2-D): the dedicated CI toolchain for the linux lanes.
#
# Every input is pinned:
#   * base image by multi-arch index digest (ubuntu:24.04 @ 2026-09-23);
#   * apt by the fixed Ubuntu noble snapshot 20260923T000000Z
#     (snapshot.ubuntu.com, release + updates + security pockets);
#   * every package by an exact `pkg=version` pin, re-verified after the
#     install with dpkg-query (the build fails on version drift).
#
# Toolchain surface (P2-D + CI residual): besides the original smoke-lane
# packages, the image carries `git` and `openjdk-17-jdk-headless` so the
# JetBrains build and smoke lanes can BOTH run on this one digest-pinned
# image: `git` supplies the `git rev-parse 'HEAD^{tree}'` lane-marker tree
# binding (stock eclipse-temurin baselines ship no git), and JDK 17 is the
# JVM the pinned Gradle wrapper (9.7.1) requires.
# The standalone kotlinc is NOT the apt `kotlin` package: the noble pin is
# 1.3.31 and cannot compile this tree's Kotlin 1.5+ stdlib calls
# (`String.lowercase`, `Char.code`). It is the pinned Kotlin release matching
# the Gradle Kotlin plugin version (apps/jetbrains/build.gradle.kts), fetched
# from the pinned release URL and verified by SHA-256 below, so the
# self-contained kotlinc smoke lane compiles exactly what Gradle compiles.
# JAVA_HOME is pinned to the JDK 17 tree so Gradle and kotlinc never depend
# on update-alternatives state. `libharfbuzz0b`,
# `fontconfig` and `fonts-dejavu-core` are the pinned headless-Swing font
# runtime the credential/parity smoke needs (JDK 17's libfontmanager loads
# libharfbuzz at class-init; without it every panel row fails closed with
# SunFontManager errors).
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
# JDK 17 is the toolchain the JetBrains build lane and the Gradle wrapper
# require; pin it explicitly instead of trusting update-alternatives order.
ENV JAVA_HOME=/usr/lib/jvm/java-17-openjdk-amd64

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
        libharfbuzz0b=8.3.0-2build2 \
        fontconfig=2.15.0-1.1ubuntu2 \
        fonts-dejavu-core=2.37-8 \
        openjdk-17-jre-headless=17.0.20.1+1-1~24.04 \
        openjdk-17-jdk-headless=17.0.20.1+1-1~24.04 \
        git=1:2.43.0-1ubuntu7.3 \
        unzip=6.0-28ubuntu4.1 \
        procps=2:4.0.4-4ubuntu3.3; \
    for spec in \
        ca-certificates=20260601~24.04.1 \
        build-essential=12.10ubuntu1 \
        curl=8.5.0-2ubuntu10.13 \
        pkg-config=1.8.1-2build1 \
        libssl-dev=3.0.13-0ubuntu3.15 \
        libharfbuzz0b=8.3.0-2build2 \
        fontconfig=2.15.0-1.1ubuntu2 \
        fonts-dejavu-core=2.37-8 \
        openjdk-17-jre-headless=17.0.20.1+1-1~24.04 \
        openjdk-17-jdk-headless=17.0.20.1+1-1~24.04 \
        git=1:2.43.0-1ubuntu7.3 \
        unzip=6.0-28ubuntu4.1 \
        procps=2:4.0.4-4ubuntu3.3; do \
        pkg="${spec%%=*}"; want="${spec#*=}"; got="$(dpkg-query -W -f='${Version}' "$pkg")"; \
        [ "$got" = "$want" ] || { echo "version drift: $pkg = $got (want $want)" >&2; exit 1; }; \
    done; \
    rm -rf /var/lib/apt/lists/*

# Standalone kotlinc, pinned to the Gradle Kotlin plugin version
# (apps/jetbrains/build.gradle.kts) instead of the apt `kotlin` package
# (noble pins 1.3.31). The compiler distribution is fetched from the pinned
# GitHub release URL and verified by SHA-256 before extraction; the extracted
# compiler is version-asserted in the same layer, so a moved release or a
# wrong pin fails the build. Both checks are re-asserted at runtime by
# `scripts/build-ci-image.sh --verify-runtime` in the `ci-image` CI pre-step.
ARG KOTLINC_VERSION=2.4.20
ARG KOTLINC_SHA256=59e9ca74c7904ef2c122b12114937673ccce68de820a663f0ed66ccf8799e0b7
RUN set -eux; \
    curl --proto '=https' --tlsv1.2 -fsSLo /tmp/kotlin-compiler.zip \
        "https://github.com/JetBrains/kotlin/releases/download/v${KOTLINC_VERSION}/kotlin-compiler-${KOTLINC_VERSION}.zip"; \
    printf '%s  %s\n' "$KOTLINC_SHA256" /tmp/kotlin-compiler.zip | sha256sum -c -; \
    unzip -q /tmp/kotlin-compiler.zip -d /opt; \
    rm -f /tmp/kotlin-compiler.zip; \
    ln -s /opt/kotlinc/bin/kotlinc /usr/local/bin/kotlinc; \
    [ "$(kotlinc -version 2>&1 | sed -n 's/^.*kotlinc-jvm \([0-9][0-9.]*\).*$/\1/p' | head -n 1)" = "$KOTLINC_VERSION" ]
