# Web companion

`by serve --app` serves a small web page at `/app/` for watching and steering branches from a phone or any browser: the branch list with live status, a branch's prompt, cost, turns, checkpoints, merge readiness, recent events and diff, the same actions `by watch` offers remotely, the inbox of questions and escalations, triggers, and a queue summary. A pairing link printed in the terminal (with a QR code) signs a phone in with a scoped token that expires; Web Push tells the phone when a branch needs you.

> **Status, 1 October 2026.** Implemented and tested hermetically: the page in headless Chromium against a real server (pairing from a link, a branch appearing and settling over the event stream, a follow-up sent from the branch page, the diff), the routes, pairing (single use, expiry, the rate limit, scopes, revocation, an event stream ended by revocation), static files and their headers, and Web Push to a local mock push service that checks the VAPID signature and decrypts each message as a browser would. **Not yet tried on a real phone, or against a real push service** (FCM, Mozilla, Apple, Windows): the encryption follows RFC 8291 and is checked by a round trip with an independent decryption, not against a browser's push stack.

## Setting it up

```sh
by serve --app                       # or "app": true in the configuration
by serve token new --link --scopes read,run --ttl 8h
#  ▄▄▄▄▄▄▄ ▄ ▄▄ ▄▄▄▄▄▄▄           a QR code, on a terminal
#  ...
# https://by.example.com/app/#pair=3f1c…   on stdout
# by serve: pairing link for phone-1a2b3c (tenant default, scopes read,run): opens once,
#   until 2026-10-01 10:40 UTC; the token it gives lasts 8h. Revoke with: token revoke phone-1a2b3c
```

Scan the code (or open the link) on the phone. The page redeems the code, keeps the token for that browser tab, and removes the code from the address bar and history.

| Command | Does |
|---|---|
| `by serve token new --link [--name N] [--tenant T] [--scopes S,...] [--repo R,...] [--ttl 24h] [--code-ttl 10m] [--public-url URL] [--no-qr]` | Record a one-time pairing code in the server's store and print its link. `--ttl` is the token's lifetime (default 24h, at most 90d); `--code-ttl` how long the link may be opened (default 10m, at most 1h). Scopes default to all four, as `token new`'s do |
| `by serve token list` | Paired tokens (active, expired, revoked, with the device that redeemed them) and links not yet opened |
| `by serve token revoke NAME [--tenant T]` | Revoke a paired token or an unopened link at once; its push subscriptions are dropped |

These read and write the server's own store: its data directory's `state.db`, or `--database`. They find it as `by serve` would (`[serve] config` in `branchyard.toml`, else the repository's `.branchyard/server`), or from `-c FILE`, `--data-dir` or `--database`; the server may be running or not. The link's base is `--public-url`, else the configuration's `public_url`, else `http(s)://<listen>` (refused when the server listens on every interface, which a phone cannot open). Configured credentials are not revocable this way: remove them from the configuration and restart, as before.

Configuration:

```json
"app": true
"app": { "push": true, "push_subject": "mailto:ops@example.com",
         "push_services": ["fcm.googleapis.com", "*.push.services.mozilla.com"],
         "vapid_key": "/etc/branchyard/vapid.pk8" }
```

| Key | Meaning |
|---|---|
| `enabled` | Default true inside the object; `--app` also turns it on |
| `push` | Send Web Push (default true). The page's in-page notices work either way |
| `vapid_key` | The VAPID private key, PKCS#8, made with mode 600 when missing. Default `<data_dir>/companion/vapid.pk8`. Servers sharing a database that all send push need the same file |
| `push_subject` | The VAPID contact, `mailto:` or `https:`. Default `public_url` when it is https, else `mailto:branchyard@localhost`; Apple's service wants a real one |
| `push_services` | Hosts a subscription may name, `host` or `*.suffix`. Default: `fcm.googleapis.com`, `*.push.services.mozilla.com`, `*.notify.windows.com`, `*.push.apple.com` |

A browser registers a service worker and receives push only in a secure context: serve over HTTPS (`--tls-cert`, or a proxy that does not buffer `text/event-stream`), or open it as `localhost`.

## Security model

**The page adds no authority.** It is static HTML, CSS and JavaScript embedded in the binary, served without a token because it contains nothing but code. Everything it shows or does is an existing API call — `GET …/branches`, the event stream, `POST …/send`, `…/steer`, `…/cancel`, `…/merge`, `…/fork`, `…/answer`, `/v1/triggers/{t}/enable` and so on — made with the caller's bearer token and checked against its scopes and tenant exactly as `by --remote` is. The new routes are `POST /app/pair`, `GET /v1/app/me` (the caller's own principal), and `/v1/app/push…` (the caller's own push subscriptions, `read` scope).

**The token lives in `sessionStorage`, sent as `Authorization: Bearer`.** The other choice was an `HttpOnly` cookie set by the pairing request. A cookie would keep the token from script, but it is an ambient credential: every API route would then also accept it, so every state-changing route would need CSRF protection, and the API's model (a bearer token or nothing) would change for every client. A header-only token cannot be sent by another site at all. The risk it takes instead, script reading the token, is closed by the page's policy: there is no script but the page's own file, server text is only ever set with `textContent`, and `connect-src 'self'` and `img-src 'self'` leave no channel to send a token elsewhere. `sessionStorage` is per tab and gone when the tab closes (the repository last chosen is kept there too); nothing goes in `localStorage` or a cookie.

**Content Security Policy**, on every file of the page:

```text
default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; connect-src 'self';
manifest-src 'self'; worker-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'
```

with `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, `X-Frame-Options: DENY`, `Cross-Origin-Opener-Policy: same-origin`, `Cross-Origin-Resource-Policy: same-origin` and a `Permissions-Policy` that turns off camera, microphone, location, payment and USB. No inline script or style, no CDN, no external font: the page works offline (the service worker keeps its files) and shows that the server cannot be reached. Only the page's exact file names are public; anything else under `/app/` needs a token like every other path. With the companion off, `/app/` is an unknown route behind the token like any other.

**Pairing.** `token new --link` stores the SHA-256 of a 128-bit random code with the principal it will create (name, tenant, scopes, repositories) and the token's lifetime. The code is in the link's fragment (`#pair=`), which browsers never send in a request line, so no access log or proxy log records it; the page sends it once in the body of `POST /app/pair` (bodies are never logged) and removes it from the address bar and history. Redemption deletes the code's row and records the token in one transaction (`DELETE … RETURNING`), so a code works once even with several servers on one database. Codes expire (default 10 minutes); redemption attempts are limited to 10 a minute per server, answered `429 rate_limited` with `Retry-After` beyond that (a limit an attacker can use to delay pairing, never to get in). The token is 256 random bits, returned once with `Cache-Control: no-store`, and stored as its SHA-256 like every credential.

**Paired tokens** carry the principal from the link, never more than the operator chose; they expire, and `token revoke` ends them at once on every server sharing the store (each request reads the token's row). An event stream opened with a paired token ends when the token expires, and within 10 seconds of a revocation. A paired token is an ordinary bearer token: `by --remote` accepts it too. `/metrics` accepts configured credentials only.

**Push.** Subscriptions are bound to the credential that made them, and sent to only while it still verifies and its principal may read the repository; a revoked or expired token's subscriptions are dropped. Endpoints must be `https://` hosts in `push_services` (an `http://` loopback literal only when listed, for tests), so a token holder cannot make the server post to an arbitrary address. Messages are encrypted for the browser (RFC 8291 `aes128gcm`: ephemeral ECDH P-256, HKDF-SHA-256, AES-128-GCM) and signed with VAPID (RFC 8292, ES256); the push service sees neither the text nor the token. A tap opens a fragment of the page, never another URL.

## What the page shows and does

| View | From | Actions |
|---|---|---|
| Branches | `GET …/branches`, then the event stream (`fetch` with the bearer header, reconnecting from the last cursor with backoff; a cursor past the end reloads) | Filter; New task (`POST …/tasks`, with the harness from `GET /v1/harnesses`) |
| Branch | `GET …/branches/{b}`, `…/event-page?limit=200` (recent events and checkpoints), `…/operations?branch=`, `…/diff` (plain monospace, added and removed lines marked, no syntax highlighting) | Send, Steer, Resume, Cancel (confirmed), Merge (confirmed), Fork: `by watch`'s rows that work remotely, under the same rules for when each applies, disabled with the reason otherwise or when the token lacks the scope |
| Inbox | Each delegating branch's `…/inbox`; answered questions found in the asker's inbox | Answer a question; Approve or Deny an escalation (an answer beginning `Approved.` or `Denied.`), as `by answer --as` |
| Triggers | `GET /v1/triggers` | Enable or disable (`POST …/enable`, `…/disable`) |
| Queue | `GET …/operations`, the branch list | Counts by status, turns and reported cost |
| Settings | `GET /v1/app/me`, `GET /v1/app/push` | Turn push on or off for this device, send a test, sign out |

`by watch`'s local-only rows (copy path, pull requests, open, rewind, try, browse, stop ports) and `review` (an editor) are not offered.

**Permission requests are shown, not answered.** A server decides each tool request at once with the policy its task or send carried (`deny` by default; there is no remote `ask`), so there is nothing pending to approve. The inbox lists the requests seen while the page is open with their decision, and Send and Fork have an "Allow every tool" box (`policy: {"mode": "allow"}`, like `--yes`) for a branch that needs what was denied.

### Notifications

The page and the server notice the same things `by watch` does, each once: a permission request (`permission`), a question or escalation (`question`), a stall (`stalled`), a failed or blocked branch (`failed`), an interrupted one (`interrupted`), and a finished turn — ready to merge, no changes, or stopped at its budget (`finished`). While the page is open they appear as notices on it (and as system notifications when the tab is hidden and the browser allows it). With push on, the server follows each repository's feed by a durable cursor (the webhook cursors' table, `<repo>:companion-push`), skips entries more than 10 minutes old (after downtime), and sends one attempt per subscription; a 404 or 410 from the push service drops the subscription. A subscription may name the kinds it wants. Several servers sharing a database share the cursor: a server claims the entries it read by moving the cursor past them with a compare-and-set before sending anything, and one that loses the race sends none of them, so each notification is sent once whichever servers have push on. A server that stops between its claim and its sends loses those notifications rather than repeating them.

## What is not done

- Not run on a real phone or against a real push service; iOS needs the page added to the home screen before it may subscribe.
- One rate limit per server process, not per client address (the server does not see client addresses behind a proxy).
- Pairing codes and tokens are in the operation store's database; there is no expiry sweep beyond the codes a new link removes, so `token list` keeps old rows.
- No push retry: a nudge that fails is logged, and the page shows the state when opened.
- A server that cannot build its HTTP client (no readable trust roots, say) starts with push off and says so in its log; the page and pairing still work.

## Code and tests

[`companion/`](../crates/branchyard-server/src/companion/mod.rs) in `branchyard-server`: routes and authentication hooks (`mod.rs`), the store on SQLite and PostgreSQL (`store.rs`, following `store.rs`'s catalog-first, one-step-per-transaction DDL under the advisory lock), pairing links and `token list`/`revoke` (`link.rs`), Web Push (`push.rs`; two servers' followers over one store sending each notice once, on SQLite in its unit tests and on PostgreSQL in `tests/postgres.rs`), the QR encoder (`qr.rs`, checked against matrices from an independent encoder made by [`tools/qr_crosscheck.js`](../tools/qr_crosscheck.js)), and the page (`assets/`). Wire types are `branchyard_client::companion`, in [`schema/contract.json`](../schema/contract.json). Tests: [`tests/companion.rs`](../crates/branchyard-server/tests/companion.rs) and [`tests/companion_browser.rs`](../crates/branchyard-server/tests/companion_browser.rs) (Playwright and Chromium; skipped, saying so, when Node or the Playwright module is missing).
