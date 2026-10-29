# Native identity and connector accounts

Branchyard separates the server caller, the process's cloud identity, and the account used by an upstream connector. Granting one does not grant the others. A Google Storage IAM permission does not become a Branchyard server scope. A branch's connector grant does not grant access to another person's Google document.

## Native Google access

Sync obtains Google API credentials through the official `google-cloud-auth` provider. Set `GOOGLE_APPLICATION_CREDENTIALS` to an ADC configuration when needed, or run with an attached Google Cloud identity. Official ADC discovery supports external-account federation and local user or impersonation configurations instead of assuming every file contains a private key. No `gcloud` subprocess or custom STS exchange is used. The existing `GOOGLE_OAUTH_ACCESS_TOKEN` override remains a fixed, externally managed token; Branchyard cannot refresh it.

GCS and Cloud KMS use the provider's access token with cloud-platform scope. Native IAM still decides whether each API request is permitted. The blocking storage interface owns a separate runtime thread with a bounded request queue and request deadlines, so calling it from a server Tokio runtime does not nest runtimes. The provider owns cache expiry and background refresh. Error messages identify the credential boundary without printing the provider's secret-bearing error response.

Explicit emulator access remains anonymous. Credential discovery is lazy: opening a store does not read an ADC file until an authenticated request needs a token. Tests construct fake credential sources and loopback endpoints.

## Google ID-token admission

A server can accept Google-signed ID tokens alongside its existing configured credentials. Configure an exact audience and exact issuer/subject bindings:

```json
{
  "google_identity": {
    "audience": "https://branchyard.example",
    "bindings": [
      {
        "issuer": "https://accounts.google.com",
        "subject": "123456789012345678901",
        "principal": {
          "name": "build-worker",
          "tenant": "engineering",
          "scopes": ["read", "run"],
          "repos": ["application"]
        }
      }
    ]
  }
}
```

The official verifier checks the Google signature, trusted Google certificates and configured audience. Branchyard also requires issue/expiry timestamps and enforces not-before when present. A presented token is bounded in size and verification has a deadline. Unknown subjects, alternate issuers and unbound audiences are rejected. Bindings do not match email addresses, token scopes, wildcard subjects or request-supplied tenants. A Google API access token is not a server ID token.

Enabling Google admission initializes the ring TLS provider when the process has no default provider, including when the verifier is used before server startup. An already installed TLS provider is preserved.

The resulting principal uses the configured scopes and repository policy. The API and main-listener `/metrics` apply the same Google admission, with `admin` still required for metrics. Google caller credentials do not authorize the companion's pairing or push routes; public pairing-code redemption remains a separate flow. A Google-only configuration does not create a fallback static token file. Arbitrary enterprise OIDC and Cloud IAM role synchronization are outside this implementation.

The principal's durable `origin` records the verified issuer, subject and server audience without the bearer. Authenticated device pairing records `origin.kind = "paired"`, preserving the original subject and issuer through further pairing. Each device keeps its own name and narrowed scopes and repository allowlist. A request, push delivery or queued operation rechecks those grants against the original issuer's current policy. Removing that configured issuer or Google binding, changing its tenant or audience, or narrowing it below the device's grants refuses the device. A queued operation reports `identity_binding_revoked`. Renaming a Google binding's display name does not change a paired device's subject. Nested pairing checks the original issuer and the leaf token. It does not retain intermediate token identifiers or inherit their later revocation.

Captured service callbacks also recheck the exact credential that authenticated their caller. Revoking that paired token remains effective even when another live token has the same principal. A trusted internal caller with no captured credential receives a current-policy check, without proof of a specific paired token. Normal paired requests and callbacks enforce token expiry. Existing durable operation admission retains its identity after token expiry and still checks issuer policy. Locally created operator links with no recorded issuing origin keep their existing expiry and revocation behavior. Older records default to `origin.kind = "configured"`. This provenance check does not enable Google machine credentials to issue browser pairing links.

## Subjects at the connector boundary

A static caller keeps its existing connector subject. The `google:` prefix is reserved; configured or paired static principals using it must be renamed, and queued work with that old ambiguous static name is refused. A Google-admitted caller uses `google:` followed by SHA-256 of the compact JSON array containing its verified external issuer and subject. This bounded subject distinguishes external callers even when configured display names coincide. Idempotency uses the derived subject too. Existing name-based connector/model/network ceilings and approval policies are applied to the corresponding derived subjects, so changing the identity representation cannot bypass them.

Branchyard signs this subject with its own issuer and `by_tenant` in turn and connect tokens. Anvil's account owner is the authenticated Branchyard issuer, tenant and signed subject. Connect-purpose and turn-purpose operations must resolve the same namespace. Ambiguous old subject-only vault records require an explicit migration; they are not automatically adopted by a newly identified caller.

The branch token remains a broker credential and is never an upstream API or MCP credential. Anvil owns human OAuth consent and refresh, workload credential selection, and protected upstream MCP execution. Missing human consent must not fall back to gateway workload authority. Native IAM, OAuth scopes and resource ACLs remain authoritative on the actual upstream call. Child connector grants can only narrow their parent's authority.

## Evidence

Hermetic evidence, 2026-10-29: `cargo test -p branchyard-server --lib --locked --offline auth::policy::tests` passed. The tests use local SQLite and constructed provenance to check issuer removal and narrowing, nested pairing, stable subjects, exact captured-token revocation, legacy operator links, paired request verification and push ownership. The queued paired-Google regression failed before the fix because removal of its binding still allowed execution. Not done in these regressions: real Google admission, Google browser pairing, cloud services or model calls.

Hermetic evidence, 2026-10-29: the server and sync library suites, server connector and tenant suites, sync conformance suite, and official-ADC integration test passed with `--locked --offline`. The regression paths use fake ADC, metadata, STS and Google certificate endpoints plus real official provider/verifier code. They exercise the blocking adapter inside a running Tokio runtime, exact identity matching, policy-controlled scopes, durable origin, invalid tokens and the HTTP API/metrics boundary. Not done in those suites: real cloud services or model calls.

Live evidence, 2026-10-29: dedicated GKE workloads exercised official ADC and native Storage IAM allow/deny through the compiled Anvil gateway and a TLS MCP qualification fixture. The complete chain also verified real Google admission, the actual engine-issued connector token, generated SDK, effect proxy, supervised Anvil and the real Storage API. Issuer, tenant and derived subject matched the gateway owner namespace; grant denial made no upstream requests. Wrong audiences and unmapped subjects were rejected. Temporary resources and private validation files were removed. See the [dated record](atomic-knowledge-validation-2026-10-29.md). Not done: live human OAuth consent for Google Workspace or a model-driven turn in that identity qualification.
