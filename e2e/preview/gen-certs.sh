#!/usr/bin/env bash
# Self-signed TLS cert for Postgres: midnight-node only talks to db-sync over TLS.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p certs
openssl req -new -x509 -days 3650 -nodes -subj "/CN=localhost" \
  -addext "subjectAltName=DNS:localhost,DNS:postgres,IP:127.0.0.1" \
  -keyout certs/server.key -out certs/server.crt
chmod 644 certs/server.crt certs/server.key   # copied and re-permissioned inside the container
echo "wrote $(pwd)/certs"
