# The self-hosted nori sync server: one static binary on an empty image.
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN cargo build --release -j4 --locked -p nori-sync-server && mkdir /data

FROM scratch
COPY --from=build /src/target/release/nori-sync-server /nori-sync-server
COPY --from=build --chown=65534:65534 /data /data
ENV NORI_SYNC_DATA=/data NORI_SYNC_PORT=8080
VOLUME /data
EXPOSE 8080
USER 65534:65534
ENTRYPOINT ["/nori-sync-server"]
