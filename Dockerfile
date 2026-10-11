# Multi-stage production Dockerfile for Omni MCP Platform
FROM node:22-alpine AS ui-builder
WORKDIR /app/ui
COPY ui/package*.json ./
RUN npm ci
COPY ui/ ./
RUN npm run build

FROM python:3.12-slim AS runner
WORKDIR /app

ENV PYTHONUNBUFFERED=1 \
    OMNI_HOST=0.0.0.0 \
    OMNI_PORT=8080 \
    OMNI_DATA_DIR=/app/data \
    OMNI_ENV=production

RUN mkdir -p /app/data

# Copy backend source
COPY pyproject.toml ./
COPY omni_mcp/ ./omni_mcp/
COPY --from=ui-builder /app/ui/dist ./ui/dist

RUN pip install --no-cache-dir .

EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
  CMD python -c "import urllib.request; urllib.request.urlopen('http://localhost:8080/healthz')" || exit 1

ENTRYPOINT ["python", "-m", "omni_mcp.server"]
CMD ["8080"]

