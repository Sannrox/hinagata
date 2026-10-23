# One static binary in an otherwise empty image. The builder is pinned by
# digest; the result makes no network calls except to configured storage.
FROM rust:1-alpine@sha256:b1d5dd5a22a09404d3e798c0cf051b689077360374eb505fa5bb93c479693d87 AS build
# The vendored protobuf compiler is a glibc binary.
RUN apk add --no-cache musl-dev gcompat ca-certificates
WORKDIR /src
COPY . .
RUN cargo build --release --locked && strip target/release/hinagata
# The runtime user's data directory: the database and local artifacts.
RUN mkdir -p /data/artifacts && chown -R 65532:65532 /data

FROM scratch
COPY --from=build /src/target/release/hinagata /hinagata
# Trust roots for HTTPS object storage.
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
COPY --from=build --chown=65532:65532 /data /data
WORKDIR /data
ENV HINAGATA_DB_PATH=/data/hinagata.db HINAGATA_ARTIFACT_DIR=/data/artifacts
USER 65532:65532
ENV HINAGATA_LISTEN=0.0.0.0:7700
EXPOSE 7700
ENTRYPOINT ["/hinagata"]
