# One image, two binaries: `api` and `mock-psp`.
FROM rust:1.86-slim-bookworm AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY migrations migrations
RUN cargo build --release --locked --bins

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 app
COPY --from=build /app/target/release/api /usr/local/bin/api
COPY --from=build /app/target/release/mock-psp /usr/local/bin/mock-psp
USER app
