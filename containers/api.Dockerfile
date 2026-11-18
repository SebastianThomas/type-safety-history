FROM rust:1.100-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY migrations ./migrations
RUN cargo build --release --locked

FROM debian:bookworm-slim
COPY --from=build /src/target/release/type-safety-history-api /usr/local/bin/api
COPY fixtures /app/fixtures
WORKDIR /app
USER 10001
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/api"]

