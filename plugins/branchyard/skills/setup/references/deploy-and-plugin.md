# Topics: deploy and plugin

## deploy

Writes `compose.yaml`, `server.json` and a `secrets/` directory (the
client token and the database password, each 0600, and a `.gitignore`)
into one directory. The server image is built from a Branchyard checkout
with `docker build -f deploy/Dockerfile -t branchyard-server .`. The
server listens on the compose network only; put a TLS proxy in front
before publishing it beyond the host.

## plugin

Installs Branchyard into each harness found on this machine, from the
manifests in `plugins/branchyard/install`: its hooks, status line, commands
and skills, byte for byte as shipped in this `by`, written or merged into
the harness's own settings (an existing key is kept, never clobbered). The
question `harnesses` picks which; `--all-detected` takes every one on
`PATH`. `by init plugin --status` says what is installed, stale or missing.
A file that differs from the shipped one is replaced only with `--force`
and the user's consent.
