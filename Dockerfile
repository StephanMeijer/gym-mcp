# syntax=docker/dockerfile:1
FROM rust:1.99-slim-bookworm AS build
WORKDIR /src
COPY . .
# Migrations are embedded at compile time (sqlx::migrate!), so no DB is needed to build.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked && cp target/release/gym-mcp /gym-mcp

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /gym-mcp /gym-mcp
EXPOSE 8080
USER nonroot
ENTRYPOINT ["/gym-mcp"]
