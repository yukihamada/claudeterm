#!/bin/sh
set -e

# Fix /data ownership so node can write to the persistent volume
chown -R node:node /data 2>/dev/null || true

# Copy global git config to node's home
cp /root/.gitconfig /home/node/.gitconfig 2>/dev/null || true
chown node:node /home/node/.gitconfig 2>/dev/null || true

# Start Litestream replication in background (runs as root to access /data)
if [ -f /etc/litestream.yml ] && command -v litestream > /dev/null 2>&1; then
  litestream replicate -config /etc/litestream.yml &
  echo "[entrypoint] litestream replication started (pid $!)"
fi

# Drop to non-root user and exec the process
exec gosu node "$@"
