# Topic: server

Writes a server configuration (default `.branchyard/server.json`), one
credential per tenant, and each credential's token in a 0600 file under
`tokens/` beside it (with a `.gitignore`). Only the token's SHA-256 goes in
the configuration, as `branchyard-server token new` does.

- Loopback (`127.0.0.1:8421`) needs no TLS. Any other address needs a
  certificate and key, or the server refuses to start; the plan's
  validation says so.
- PostgreSQL: the URL must not hold a password. Use a `.pgpass` file, or
  the `deploy` topic, which reads the password from a secret file.
- Tenancy `multi` asks for tenant names and one quota for each: turns
  running at once, open branches, dollars reserved.
- Providers (`microsandbox`, `substrate`), delegation and secrets are
  refused by the server unless allowed here. Secrets map a name to the
  server's own variable or `@file`, never a value.
- A token file that already exists is kept, never read into the plan:
  re-running keeps clients working.

After applying: `by serve --config <path> --check`, then
`by serve --config <path>`. Clients use `--remote URL --token-file FILE`
(or `[remote]` in `branchyard.toml`, from `by init project`).
