#!/usr/bin/env bash
# Runs the tests that need Postgres: the store's own, and the end-to-end ones
# over it. They get a throwaway Postgres in Docker, on a loopback port, with
# TLS from a root made for this run. Nothing outlives the run, however it
# ends: not the container, not the certificates.
set -euo pipefail

image=postgres:18.1-trixie
password=not-a-secret
name="mensor-test-postgres-$$"
work=$(mktemp -d)
# A run that fails shows what Postgres said, then everything goes.
cleanup() {
  status=$?
  if [ "$status" -ne 0 ] && docker inspect "$name" >/dev/null 2>&1; then
    echo "--- Postgres's log, as the run failed (exit $status):" >&2
    docker logs --tail 60 "$name" >&2 || true
  fi
  docker rm -f "$name" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

# A root, and the server's certificate it signs, for localhost and 127.0.0.1.
quiet() { "$@" >/dev/null 2>&1; }
quiet openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 1 \
  -subj /CN=mensor-test-root -keyout "$work/root.key" -out "$work/root.pem"
quiet openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -subj /CN=localhost -keyout "$work/server.key" -out "$work/server.csr"
printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n' >"$work/server.ext"
quiet openssl x509 -req -in "$work/server.csr" -CA "$work/root.pem" -CAkey "$work/root.key" \
  -CAcreateserial -days 1 -extfile "$work/server.ext" -out "$work/server.pem"

docker pull --quiet "$image" >/dev/null

# The container's root copies the key where postgres, and only it, reads it.
docker run -d --name "$name" -e POSTGRES_PASSWORD="$password" -p 127.0.0.1::5432 \
  -v "$work:/certs:ro" --entrypoint sh "$image" -c '
    install -o postgres -m 600 /certs/server.key /tmp/server.key
    install -o postgres -m 644 /certs/server.pem /tmp/server.pem
    exec docker-entrypoint.sh postgres -c ssl=on \
      -c ssl_cert_file=/tmp/server.pem -c ssl_key_file=/tmp/server.key' >/dev/null

# Ready once it answers over TCP: its first start, which sets it up, listens
# on its socket only.
for _ in $(seq 300); do
  if docker exec "$name" pg_isready -q -h 127.0.0.1 -U postgres; then break; fi
  sleep 0.2
done
docker exec "$name" pg_isready -q -h 127.0.0.1 -U postgres
port=$(docker port "$name" 5432/tcp | head -1 | sed 's/.*://')

# The end-to-end tests name their databases; the store's make their own.
for database in e2e_restart e2e_wait e2e_from; do
  docker exec "$name" createdb -U postgres "$database"
done

export MENSOR_TEST_POSTGRES="postgres://postgres:$password@127.0.0.1:$port"
export MENSOR_TEST_POSTGRES_ROOT="$work/root.pem"
cargo test --locked -p mensor-store-postgres -- --include-ignored
cargo test --locked -p mensor --features postgres --test e2e --test run -- --include-ignored postgres
