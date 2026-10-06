# build from the repo root:  docker build -f docker/Dockerfile -t atlas .
# (docker/docker_compose.yml does this for you)
# keep in step with rust-toolchain.toml
FROM rust:1.99.0-slim-trixie AS build

WORKDIR /src

# deps first soo they stay cached while only src/ changes
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && touch src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
RUN touch src/main.rs src/lib.rs && cargo build --release --locked


FROM debian:trixie-slim

COPY --from=build /src/target/release/atlas /usr/local/bin/atlas
COPY docker/docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh

ENV ATLAS_HOME=/app/data

WORKDIR /app
VOLUME ["/app/data"]

ENTRYPOINT ["docker-entrypoint.sh"]
