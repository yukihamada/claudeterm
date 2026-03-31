FROM node:22-slim

# Install claude CLI
RUN npm install -g @anthropic-ai/claude-code

# Basic dev tools users might need
RUN apt-get update && apt-get install -y --no-install-recommends \
    git curl ca-certificates python3 \
    && rm -rf /var/lib/apt/lists/*

RUN git config --global user.name "CodePal User" && \
    git config --global user.email "user@codepal.dev" && \
    git config --global init.defaultBranch main

WORKDIR /workspace
