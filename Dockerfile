# --- build : musl statique pour une image scratch (~0 dépendance runtime) ---
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# rust:alpine = musl natif -> binaire statique pour l'arch courante (amd64 OU arm64,
# donc compatible buildx multi-arch sans cible codée en dur)
RUN cargo build --release && cp target/release/l4dder-worker /worker

# --- runtime : scratch + juste les certifs CA pour le TLS ---
FROM scratch
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build /worker /worker
# tourne en non-root
USER 65534:65534
ENTRYPOINT ["/worker"]
