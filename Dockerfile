# syntax=docker/dockerfile:1
# Static Linux build of rusty, plus a small runtime image.
#   docker build -t rusty .                       # runtime image
#   docker build --target bin -o out .            # just the binary, in ./out
FROM rust:1-alpine AS build
RUN apk add --no-cache gcc musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY xtask xtask
COPY skills skills
# Keep dependencies and compiler artifacts across source edits. Copy the
# deliverables out before the target cache is unmounted.
RUN --mount=type=cache,id=rusty-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=rusty-release-target,target=/src/target,sharing=locked \
    cargo build --release --locked --bins && mkdir -p /out \
    && cp target/release/rusty target/release/rusty-memoryd target/release/rusty-cloud /out/ \
    && strip /out/rusty /out/rusty-memoryd /out/rusty-cloud

FROM scratch AS bin
COPY --from=build /out/rusty /rusty
COPY --from=build /out/rusty-memoryd /rusty-memoryd
COPY --from=build /out/rusty-cloud /rusty-cloud
COPY LICENSE THIRD_PARTY_NOTICES.txt /

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends git ripgrep ca-certificates python3 openssh-client \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /out/rusty /usr/local/bin/rusty
COPY --from=build /out/rusty-memoryd /usr/local/bin/rusty-memoryd
COPY --from=build /out/rusty-cloud /usr/local/bin/rusty-cloud
COPY LICENSE THIRD_PARTY_NOTICES.txt /usr/share/doc/rusty/
WORKDIR /work
ENTRYPOINT ["rusty"]
