# openai-codex-proxy

A standalone proxy that makes **Codex CLI** see a local service as an OpenAI-compatible Responses backend while the proxy routes requests across one or more **Codex subscription / ChatGPT-authenticated accounts**.

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
- request-time rewrite for stale unsupported model slugs using the local model cache
- a direct CLI helper for login and account listing

It still does **not** yet include:

- live usage polling from Codex backend
- per-user account ownership and downstream authn/authz
- sticky routing/session affinity

## Main way to use it

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

### Start the proxy
```bash
cargo run
```

or

```bash
cargo run -- serve
```

## HTTP API

### Health

- `GET /health`

### Accounts

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
- `OCP_DATA_DIR` — default `./data`
- `OCP_REQUEST_TIMEOUT_SECS` — default `600`

The upstream Responses endpoint is hardcoded to:

```text
https://chatgpt.com/backend-api/codex/responses
```

## Quick start

```bash
cd ~/Projects/openai-codex-proxy
cargo run -- login --browser
cargo run -- login --device-auth
cargo run -- list-accounts
cargo run
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

## Next implementation steps

1. Poll Codex usage/rate limits and update account routing scores live.
2. Add downstream authentication and user ownership of accounts.
3. Add sticky routing/session affinity.
4. Add account policy controls.
