# Rust core — multi-stage. Migrations are embedded at compile time (sqlx::migrate!).
FROM rust:1.96-bookworm AS build
WORKDIR /src
COPY Cargo.toml ./
COPY src ./src
COPY migrations ./migrations
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/yw-core /usr/local/bin/yw-core
EXPOSE 8080
CMD ["yw-core"]
