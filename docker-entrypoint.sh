#!/bin/sh
set -e

# Fix /data ownership so node can write to the persistent volume
chown -R node:node /data 2>/dev/null || true

# Copy global git config to node's home
cp /root/.gitconfig /home/node/.gitconfig 2>/dev/null || true
chown node:node /home/node/.gitconfig 2>/dev/null || true

# Drop to non-root user and exec the process
exec gosu node "$@"
