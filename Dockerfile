# Multi-stage production Dockerfile for Omni MCP Platform (Rust 2.0 Engine)
FROM node:22-alpine AS ui-builder
WORKDIR /app/ui
COPY ui/package*.json ./
RUN npm ci
COPY ui/ ./
RUN npm run build

FROM rust:1.85-slim AS rust-builder
WORKDIR /app
COPY Cargo.toml ./
COPY src/ ./src/
RUN cargo build --release

FROM debian:bookworm-slim AS runner
LABEL org.opencontainers.image.version="4.7.0"
WORKDIR /app

ENV OMNI_HOST=0.0.0.0 \
    OMNI_PORT=8080 \
    OMNI_DATA_DIR=/app/data \
    OMNI_ENV=production

RUN mkdir -p /app/data

COPY --from=rust-builder /app/target/release/omni-gateway /usr/local/bin/omni-gateway
COPY --from=rust-builder /app/target/release/omni /usr/local/bin/omni
COPY --from=rust-builder /app/target/release/aifx /usr/local/bin/aifx
COPY --from=ui-builder /app/ui/dist ./ui/dist

EXPOSE 8080

ENTRYPOINT ["omni-gateway"]
CMD ["8080"]
