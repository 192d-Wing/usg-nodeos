# EST Enrollment

Node identity enrollment uses EST (RFC 7030). The node starts from a minimal
YAML configuration at `/etc/nodeos/noded.yaml`.

```yaml
listenAddr: "[::1]:9443"
nodeId: qemu-node-001
# Enrolled identity on writable state (root is read-only); trust anchor in /etc.
certFile: /var/lib/nodeos/pki/server.crt
keyFile: /var/lib/nodeos/pki/server.key
clientCa: /var/lib/nodeos/pki/client-ca.crt
enrollment:
  est:
    serverUrl: https://[2001:db8::10]
    bearerToken: replace-with-bootstrap-token
    caCertFile: /etc/nodeos/pki/est-ca.crt
    # label: issuing-ca   # optional RFC 7030 CA label
  # renewCheckIntervalSecs: 3600   # optional; renewal-loop cadence (default 3600)
```

`serverUrl` is the **base** EST URL; the client appends
`/.well-known/est/<operation>` (or `/.well-known/est/<label>/<operation>` when an
optional `label` is configured).

## Shared EST client

The EST protocol is implemented by the shared
[`usg-est-client`](https://github.com/192d-Wing/usg-est-client) crate (pinned to
tag `v2.0.0`) so that the node OS does not maintain a second EST implementation.
`noded` only adapts its YAML config into that crate's `EstClientConfig`, drives
the enrollment, and persists the resulting PKI. See `crates/noded/src/est.rs`.

## FIPS-validated cryptography

`noded` has a `fips` cargo feature, **on by default**. It enables
`usg-est-client/fips` + `rustls/fips`, routing both the mTLS server and the EST
client's TLS through the aws-lc-rs FIPS module. At startup `noded` installs that
provider process-wide and **fails closed** — it refuses to start unless the
binary is actually linked against the FIPS module.

Building the FIPS feature requires the aws-lc-rs FIPS build toolchain in the
build environment: `cmake`, Go (>= 1.18), and a C/C++ toolchain (`clang` /
`libclang`). Dev boxes or CI without that toolchain can build with
`cargo build --no-default-features` (non-validated aws-lc-rs provider; logs a
warning at startup).

> A `fips` build *links* the FIPS-validated module but is not, by itself, a
> CMVP-validated operating environment. Confirm the validated module version and
> operating conditions for an ATO (see the crate's `docs/fips-compliance.md`).

## Startup flow

On startup `noded`:

1. Loads `/etc/nodeos/noded.yaml`.
2. If `certFile`, `keyFile`, and `clientCa` are **all present**, it evaluates
   renewal (see below) and then starts the mTLS API. A present-but-invalid set
   surfaces as a TLS load error rather than silently overwriting operator
   material.
3. If any of those files are **missing**, it performs bootstrap EST enrollment:
   - pins TLS trust for the EST connection to `caCertFile` only
     (`trust_explicit`); the system/WebPKI trust stores are not used;
   - sends `Authorization: Bearer <bearerToken>` to the EST endpoints;
   - fetches the management CA bundle from `/.well-known/est/cacerts`;
   - generates a node key pair locally and builds a PKCS#10 CSR
     (`CN`/SAN = `nodeId`, server + client auth EKU);
   - submits the CSR to `/.well-known/est/simpleenroll`;
   - writes the issued certificate, the generated key, and the CA bundle
     atomically under `/etc/nodeos/pki` (temp file + fsync + rename; the key is
     created mode `0600`);
   - then starts the mTLS API.

The issued node certificate is used as the mTLS **server** certificate
(`certFile`/`keyFile`); the fetched CA bundle is the mTLS **client** verifier
(`clientCa`).

If the EST server returns "pending" (manual approval), bootstrap fails with a
diagnostic; the node retries on the next start.

## Renewal

When PKI is already present, `noded` checks the node certificate's validity
window and renews when it is past **two-thirds of its lifetime** (or already
expired). This adapts to short-lived certificates without a configured window.

Renewal:

- generates a fresh key pair and CSR (same identity);
- submits to `/.well-known/est/simplereenroll`, authenticating with the
  **current node certificate over mTLS** — no bearer token;
- atomically replaces `certFile` + `keyFile`. The CA bundle (`clientCa`) is left
  untouched; renewal rotates the leaf only.

Renewal happens both at startup (before binding the API) and continuously:

- **At startup**, best-effort — if it fails, `noded` logs a warning and starts
  with the existing (still-valid) certificate rather than refusing to boot.
- **Live**, via a background loop that wakes every `renewCheckIntervalSecs`
  (default 3600), renews when due, and **hot-swaps** the rotated certificate into
  the running mTLS server with no restart. New TLS handshakes immediately use the
  new certificate; established connections keep theirs. This is implemented with
  a swappable rustls server-certificate resolver, so only the leaf rotates — the
  client-certificate verifier (the `clientCa` bundle) is unchanged. Loop failures
  are logged and retried on the next tick; they never take the server down.

## Bearer token scope

The bearer token is **bootstrap-only**. It is sent solely to EST endpoints and
never authorizes normal management API calls. Once the node holds a certificate,
all management traffic — including renewal via `simplereenroll` — uses mTLS and
the token is unused.
