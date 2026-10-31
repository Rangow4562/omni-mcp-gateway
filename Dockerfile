# Multi-stage production Dockerfile for Omni MCP Platform (Omni Rust Engine)
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
LABEL org.opencontainers.image.version="4.7.1"
WORKDIR /app

ENV OMNI_HOST=0.0.0.0 \
    OMNI_PORT=8080 \
    OMNI_DATA_DIR=/app/data \
    OMNI_ENV=production

RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates && rm -rf /var/lib/apt/lists/*
RUN groupadd -g 10001 omni && useradd -u 10001 -g omni -m -d /app omni
RUN mkdir -p /app/data && chown -R omni:omni /app

COPY --from=rust-builder /app/target/release/omni-gateway /usr/local/bin/omni-gateway
COPY --from=rust-builder /app/target/release/omni /usr/local/bin/omni
COPY --from=rust-builder /app/target/release/aifx /usr/local/bin/aifx
COPY --from=ui-builder /app/ui/dist ./ui/dist

USER omni:omni

EXPOSE 8080

HEALTHCHECK --interval=10s --timeout=3s --start-period=5s --retries=3 \
  CMD curl -f http://127.0.0.1:8080/healthz || exit 1

ENTRYPOINT ["omni-gateway"]
CMD ["8080"]
