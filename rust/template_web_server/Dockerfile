# Build context: the repo root (`make docker_build`), because the image needs
# both the Rust workspace and the TypeScript bundler. Ignore rules live next to
# this file in Dockerfile.dockerignore.
#
# The Rust and CSS stages run natively on the build machine ($BUILDPLATFORM:
# arm64 on Todd's Mac, amd64 on a Linux box). The binary is cross-compiled to a
# static-pie x86_64-unknown-linux-musl executable, the same target the old
# emulated alpine build produced. zig is only the C compiler (for aws-lc-sys,
# ring, zstd-sys and the like); rustc links with rust-lld and its own
# self-contained musl CRT objects, which zig's linker can't do for static-pie
# (and zig's linker would also turn rustc's `--strip-debug` into a full strip,
# dropping the function names in panic and Sentry backtraces). cargo-chef
# caches the dependency build in its own layer, so a source change rebuilds
# just the workspace crates.
#
# The static assets stage stays on the target platform (linux/amd64), because
# it runs the server binary itself (`gen-static-assets`, `gen-static-scripts`).

ARG TARGET=x86_64-unknown-linux-musl

#########
# Tools #
#########

FROM --platform=$BUILDPLATFORM rust:1.100.0-alpine3.24 AS tools

ARG TARGET
ARG BUILDARCH
ARG ZIG_VERSION=0.17.1
ARG ZIG_SHA256_ARM64=a1546d5646eb184437918d6635ca6b762a0e889eac09cef720ee7a1ed3fe7795
ARG ZIG_SHA256_AMD64=3581863e3cbcfd0fce0efc682ce7e9e93da96668a977791d356a26d2a7e9b17a
ARG CARGO_CHEF_VERSION=0.1.78

RUN apk add --no-cache musl-dev xz

# zig, from the official tarball, checked against its published sha256
RUN case "$BUILDARCH" in \
      arm64) arch=aarch64; sum="$ZIG_SHA256_ARM64" ;; \
      amd64) arch=x86_64; sum="$ZIG_SHA256_AMD64" ;; \
      *) echo "unsupported build arch: $BUILDARCH" >&2; exit 1 ;; \
    esac \
 && wget -q -O /tmp/zig.tar.xz "https://ziglang.org/download/${ZIG_VERSION}/zig-${arch}-linux-${ZIG_VERSION}.tar.xz" \
 && echo "$sum  /tmp/zig.tar.xz" | sha256sum -c - \
 && mkdir /opt/zig \
 && tar -xJf /tmp/zig.tar.xz -C /opt/zig --strip-components=1 \
 && rm /tmp/zig.tar.xz
ENV PATH="/opt/zig:${PATH}"

RUN cargo install --locked cargo-chef@${CARGO_CHEF_VERSION} \
 && rm -rf "$CARGO_HOME/registry"
RUN rustup target add ${TARGET}

# zig as the C compiler and archiver for the target. cc-rs sees a clang and
# adds its own `--target=x86_64-unknown-linux-musl`, which zig doesn't know, so
# the wrapper drops it and passes zig's own target instead. zig cc also turns
# on UBSan for some objects and expects to link its own runtime for it; rustc
# links here, so sanitizers are off, as in a plain gcc build.
RUN printf '%s\n' '#!/bin/sh' \
      'for a in "$@"; do shift; case "$a" in --target=*) ;; *) set -- "$@" "$a" ;; esac; done' \
      'exec zig cc -target x86_64-linux-musl -fno-sanitize=all "$@"' > /usr/local/bin/x86_64-linux-musl-zigcc \
 && printf '%s\n' '#!/bin/sh' 'exec zig ar "$@"' > /usr/local/bin/x86_64-linux-musl-zigar \
 && chmod +x /usr/local/bin/x86_64-linux-musl-zigcc /usr/local/bin/x86_64-linux-musl-zigar
ENV CC_x86_64_unknown_linux_musl=/usr/local/bin/x86_64-linux-musl-zigcc \
    AR_x86_64_unknown_linux_musl=/usr/local/bin/x86_64-linux-musl-zigar

# rustc links: rust-lld with the self-contained musl CRT (rcrt1.o, libc.a,
# libunwind.a) shipped with the target, position independent and statically
# linked, so the binary is static-pie (ASLR). rust-lld honours `--strip-debug`
# (the release profile's default strip), so the symbol table stays. A GNU
# build ID (a hash of the output, so still reproducible) is added, as gcc did,
# for matching debug files in Sentry.
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C linker-flavor=ld.lld -C link-self-contained=yes -C relocation-model=pie -C target-feature=+crt-static -C link-arg=--build-id=sha1"

WORKDIR /toddstack/rust

##########
# Recipe #
##########

# The whole workspace: Cargo refuses to build if any workspace member's
# manifest is missing, so copying crates one by one breaks the moment a crate
# is added. cargo-chef reads every member into one recipe.
FROM tools AS planner

COPY rust/ ./
RUN cargo chef prepare --recipe-path recipe.json

##########
# Binary #
##########

FROM tools AS binary_builder

ARG TARGET

# Dependencies only: this layer is reused until a manifest or Cargo.lock
# changes.
COPY --from=planner /toddstack/rust/recipe.json recipe.json
RUN cargo chef cook --release --target ${TARGET} --recipe-path recipe.json --package template-web-server --bin template-web-server

COPY rust/ ./

# generate binary
RUN cargo build --release --target ${TARGET} --package template-web-server --bin template-web-server \
 && cp target/${TARGET}/release/template-web-server /template-web-server-bin

#######
# CSS #
#######

# Sass is plain JavaScript, so its output does not depend on the platform.
FROM --platform=$BUILDPLATFORM node:26.7.0-alpine3.24 AS css_builder

# update alpine linux dependencies
RUN apk update
RUN apk add --no-cache make

# install Sass
RUN npm install -g sass@1.103.1

WORKDIR /toddstack

# copy required files
COPY Makefile .
COPY rust/template_web_server/static/scss/ rust/template_web_server/static/scss/

# generate stylesheet(s)
RUN make gen_css

##################
# Static Assets  #
##################

# One stage, not three. The generator is a static musl binary, so it runs
# happily inside the Deno image, which keeps the whole pipeline in one place.
#
# The order is load-bearing: the JavaScript build INLINES the manifest, so
# hashing has to happen first; and the built JavaScript can only be hashed once
# it exists, so the scripts are a second pass. Scripts never need their own
# hash — the layout reads that from the manifest.
FROM denoland/deno:alpine-2.9.5 AS static_builder

RUN apk update
RUN apk add --no-cache make

WORKDIR /toddstack

COPY Makefile ./
COPY typescript/ typescript/
COPY rust/template_web_server/html/ rust/template_web_server/html/
COPY rust/template_web_server/static/ rust/template_web_server/static/
COPY --from=css_builder /toddstack/rust/template_web_server/bin/static/stylesheet/ rust/template_web_server/bin/static/stylesheet/
COPY --from=binary_builder /template-web-server-bin rust/template_web_server/bin/template-web-server

RUN make gen_static \
 && make gen_static_assets \
 && make gen_js \
 && make gen_static_scripts

#######################
# Template Web Server #
#######################

FROM alpine:3.24.1

# update alpine linux dependencies
RUN apk update

WORKDIR /template-web-server

# The runtime tree is exactly what the pipeline produced: hashed assets, the
# manifest that names them, the templates, and the binary. Nothing is generated
# or renamed at startup, so this filesystem is never written to — the image can
# be read-only, and an in-place restart cannot corrupt it.
COPY --from=static_builder /toddstack/rust/template_web_server/bin/ ./

# run server
EXPOSE 8080
ENTRYPOINT ["./template-web-server"]
