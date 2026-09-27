# Topics: deploy and plugin

## deploy

Writes `compose.yaml`, `server.json` and a `secrets/` directory (the
client token and the database password, each 0600, and a `.gitignore`)
into one directory. The server image is built from a Branchyard checkout
with `docker build -f deploy/Dockerfile -t branchyard-server .`. The
server listens on the compose network only; put a TLS proxy in front
before publishing it beyond the host.

## plugin

Installs the `setup` and `delegate` skills, byte for byte as shipped in
this `by`, into a project's `.claude/skills`, the user's
`~/.claude/skills`, or `~/.codex/skills`; or prints the command that loads
the whole plugin (`claude --plugin-dir …`). An install that differs from
the shipped skill is replaced only with `--force` and the user's consent.
