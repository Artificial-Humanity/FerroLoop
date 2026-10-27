# syntax=docker/dockerfile:1
FROM node:22-bookworm-slim AS ui-builder

WORKDIR /ui
COPY ui/package.json ui/package-lock.json* ./
RUN npm ci || npm install
COPY ui/ ./
RUN npm run build

FROM rust:1.98-bookworm AS builder

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN cargo build --release --package fl-cli --bin fl

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git util-linux \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/fl /usr/local/bin/fl
COPY --from=ui-builder /ui/dist /usr/share/ferroloop/ui
RUN ln -s /usr/local/bin/fl /usr/local/bin/agent-msg \
    && ln -s /usr/local/bin/fl /usr/local/bin/agent-bus-mcp \
    && ln -s /usr/local/bin/fl /usr/local/bin/agent-bus-claude-hook

WORKDIR /workspace
EXPOSE 8420

ENTRYPOINT ["/usr/local/bin/fl"]
CMD ["hub", "serve", "--listen", "0.0.0.0:8420", "--ui-dir", "/usr/share/ferroloop/ui"]
