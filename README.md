# openai-codex-proxy

A standalone proxy that makes **Codex CLI** see a local service as an OpenAI-compatible Responses backend while the proxy routes requests across one or more **Codex subscription / ChatGPT-authenticated accounts**.

## Installation

### Prerequisites

- Rust toolchain with `cargo`
- a local Codex CLI installation, because this proxy reads Codex auth and model cache data

### Install from git

```bash
cargo install --locked --git https://github.com/ravshansbox/openai-codex-proxy.git openai-codex-proxy
```

This installs the `openai-codex-proxy` binary into Cargo's bin directory, typically `~/.cargo/bin`.
Using `--locked` keeps installation reproducible and avoids pulling newer transitive dependency revisions than this repository was tested with.

### Run from source instead

```bash
git clone https://github.com/ravshansbox/openai-codex-proxy.git
cd openai-codex-proxy
cargo run -- --help
```

If you installed the binary with `cargo install`, use `openai-codex-proxy ...`.
If you are running from a checkout, use `cargo run -- ...`.

## Quick start

Installed binary:

```bash
openai-codex-proxy set-api-key sk-local
openai-codex-proxy login --browser
openai-codex-proxy list-accounts
openai-codex-proxy serve
```

Run from source:

```bash
cargo run -- set-api-key sk-local
cargo run -- login --browser
cargo run -- list-accounts
cargo run -- serve
```

## What this project is for

Downstream, Codex CLI talks to this proxy like a normal OpenAI-style provider:

- `GET /v1/models`
- `POST /v1/responses`

Upstream, the proxy manages **real Codex OAuth accounts** and selects one authenticated account for each request.

## Current implementation

This scaffold now includes:

- an `axum` HTTP server
- persistent account registry under `data/accounts.json`
- per-account auth homes under `data/accounts/<account-id>/`
- browser OAuth login
- device-code OAuth login
- login status polling
- `GET /v1/models` from the local Codex model cache
- `POST /v1/responses` proxying using stored OAuth state
- automatic routing across all authenticated accounts
- single proxy API key protection for `/v1/*`
- request-time rewrite for stale unsupported model slugs using the local model cache
- a direct CLI helper for login, account listing, and proxy API key setup

It still does **not** yet include:

- per-user account ownership and downstream authn/authz
- sticky routing/session affinity

## Main way to use it

The examples below use `cargo run -- ...`. If you installed the binary, replace that prefix with `openai-codex-proxy`.

### Log in an account in browser
```bash
cargo run -- login --browser
```

### Or use device code
```bash
cargo run -- login --device-auth
```

### List accounts
```bash
cargo run -- list-accounts
```

Default output is compact: email plus usage windows.

Verbose output:

```bash
cargo run -- list-accounts --verbose
```

### Set the proxy API key
```bash
cargo run -- set-api-key
```

Or provide an explicit key:

```bash
cargo run -- set-api-key sk-local
```

Check whether one is configured:

```bash
cargo run -- api-key-status
```

If you change the key while the proxy is already running, restart the proxy so the new key is picked up.

### Start the proxy
```bash
cargo run
```

or

```bash
cargo run -- serve
```

All management and API endpoints except `/health` require:

```text
Authorization: Bearer <proxy-api-key>
```

## HTTP API

### Health

- `GET /health`

### Accounts

These endpoints require the proxy API key.

- `GET /accounts`
- `POST /accounts`
- `GET /accounts/:account_id`
- `DELETE /accounts/:account_id`

Create account body:

```json
{
  "preference": 10
}
```

### Login flows

These endpoints also require the proxy API key.

Recommended one-step endpoints:

- `POST /accounts/login/browser/start`
- `POST /accounts/login/device-code/start`

These endpoints no longer require you to provide account names or tags.

Lower-level endpoints still exist if you want to attach OAuth to an already-created local account record:

- `POST /accounts/:account_id/login/browser/start`
- `POST /accounts/:account_id/login/device-code/start`

And for polling/cancel:

- `GET /logins/:login_id`
- `POST /logins/:login_id/cancel`

### Models and Responses

- `GET /v1/models`
- `POST /v1/responses`

Optional routing header:

- `x-codex-account-id: <account-id>`

If you do not set it, the proxy automatically picks the best authenticated account it has.
If the incoming request uses a stale model slug that is not present in the local Codex `models_cache.json`, the proxy rewrites it to the best currently supported cached model.

## Configuration

Environment variables:

- `OCP_LISTEN_ADDR` — default `127.0.0.1:8080`
- storage path is fixed at `~/.openai-codex-proxy`
- `OCP_REQUEST_TIMEOUT_SECS` — default `600`

The upstream Responses endpoint is hardcoded to:

```text
https://chatgpt.com/backend-api/codex/responses
```

## Codex CLI configuration example

Point Codex CLI at the proxy as a custom model provider:

```toml
model_providers.codex-proxy = { name = "Codex Proxy", base_url = "http://127.0.0.1:8080/v1", wire_api = "responses" }
model_provider = "codex-proxy"
```

Or on the CLI:

```bash
codex exec \
  -c "model_providers.codex-proxy={ name='Codex Proxy', base_url='http://127.0.0.1:8080/v1', wire_api='responses' }" \
  -c model_provider='codex-proxy' \
  'Hello through the proxy'
```

## Release

GitHub Releases are built from tags that start with `v`, for example:

```bash
git tag v0.1.0
git push origin v0.1.0
```

The release workflow will build tarballs for Linux and macOS.
