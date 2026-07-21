# --- build : musl statique pour une image scratch (~0 dépendance runtime) ---
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml ./
COPY src ./src
# cible musl -> binaire 100% statique, aucune libc dynamique
RUN rustup target add x86_64-unknown-linux-musl && \
    cargo build --release --target x86_64-unknown-linux-musl && \
    cp target/x86_64-unknown-linux-musl/release/l4dder-worker /worker

# --- runtime : scratch + juste les certifs CA pour le TLS ---
FROM scratch
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build /worker /worker
# tourne en non-root
USER 65534:65534
ENTRYPOINT ["/worker"]
