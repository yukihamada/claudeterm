FROM rust:1.88-bookworm AS chef
RUN cargo install cargo-chef
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release --bin claudeterm

# ── base: apt + npm install (cached unless this layer changes) ──────────────
FROM node:22-slim AS base
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl git python3 python3-pip gosu ffmpeg \
    && pip3 install --break-system-packages google-genai \
    && rm -rf /var/lib/apt/lists/*
# Pin version so cache is stable; bump manually when upgrading claude CLI
RUN npm install -g @anthropic-ai/claude-code@1
# Deploy CLIs (users provide tokens via Keys vault)
RUN curl -L https://fly.io/install.sh | FLYCTL_INSTALL=/usr/local sh \
    && npm install -g vercel@latest wrangler@latest netlify-cli@latest @railway/cli@latest

# ── runtime: copy binary on top of cached base ──────────────────────────────
FROM base AS runtime

# Configure git (required by claude)
RUN git config --global user.name "Claude Code" && \
    git config --global user.email "user@claudeterm.app" && \
    git config --global init.defaultBranch main && \
    git config --global --add safe.directory '*'

# node:22-slim already has a 'node' user (uid 1000) — reuse it as our non-root runner
RUN mkdir -p /data/workspaces /home/node && chown node:node /home/node

COPY --from=builder /app/target/release/claudeterm /usr/local/bin/claudeterm

# Entrypoint: fix /data ownership at startup (volume mounts as root), then drop to codeuser
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

ENV PORT=3000
ENV WORKDIR=/data/workspaces
ENV CLAUDE_COMMAND=/usr/local/bin/claude
ENV HOME=/home/node

EXPOSE 3000
ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
CMD ["claudeterm"]
