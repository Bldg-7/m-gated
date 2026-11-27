# m-gated

m-gated is a self-hosted LLM API gateway with a strict cost ledger. Clients use the official
Anthropic, OpenAI, DeepSeek and TypeSafe SDKs against the gateway, and the gateway forwards their requests
to the real provider with a provider key that only the server holds (stored encrypted in Postgres
and managed from the admin dashboard). Before a request is sent, the
gateway checks it against the caller's policies. Afterwards it prices the request and records the
cost in a Postgres ledger.

Key properties:

- **Native wire formats.** The request body that passes validation is forwarded byte for byte, and
  native JSON/SSE responses come back unchanged. The gateway does not translate between providers.
  The one exception is a zstd-compressed TypeSafe request body: the gateway decompresses it to
  check it and forwards the decompressed bytes.
- **Allow-list only.** A model can be used only if the operator declared a *profile* for it,
  with exact fields, limits and rates. m-gated ships **no price table**. All rates come from the
  operator.
- **Fail-closed accounting.** The gateway settles a request from the usage in the provider's
  response. If it cannot verify that usage, it records the cost as `unknown`, never as 0, and the
  user cannot make paid calls until an admin settles that request. Two kinds of request are
  exceptions (details in [Cost ledger](#cost-ledger)):
  - An Anthropic stream cut off after its `message_start` event is settled by the cut-off rule
    that the admin chose (`received` or `max_tokens`).
  - A request is settled at 0 USD if the provider answered with an HTTP error, or if it was cut off
    before `message_start` in an Anthropic stream, before the first event in another stream, or
    before the full response when not streaming.
- **Sign-in: SSO or e-mail and password.** Each gateway uses one of two ways for the dashboard,
  set by `auth.mode`: RS256 JWTs from an external SSO (verified against a JWKS), or e-mail and
  password with admin invitations sent by e-mail. From the dashboard users issue scoped virtual keys,
  which are the only credentials accepted for inference.
- **Single binary.** Written in Rust (Tokio, axum, reqwest, sqlx). It serves the React dashboard at
  `/dashboard/` and listens on port `4000`.

## Concepts

| Term | Meaning |
|---|---|
| Principal | An identity that makes requests: a *human* (a person who signs in through the SSO or with a password) or a *service* (a non-human account with a human owner, created by an admin). |
| Profile | An operator-declared entry in the config for one model and API, with its exact limits and rates. Requests for anything without a profile are rejected. |
| Policy | Per-principal (and per-API-key) settings managed by admins: **grants** plus optional **limits**. |
| Grant | A tuple of provider, model and *capabilities*. A capability is one API family: `messages`, `count_tokens`, `responses`, `chat_completions` or `embeddings`. |
| Limits | A monthly USD budget, `rpm` (requests per minute), `tpm` (tokens per minute) and `count_rpm` (count_tokens requests per minute). |
| Execution ceiling | Optional server-wide limits in the config. They apply on top of every policy. |
| Generating API | Any API whose requests set an output limit: everything except `count_tokens`, `embeddings` and `systemone`. |

## Supported APIs

| Provider | Gateway route | Profile `api` | Grant capability | SDK base URL |
|---|---|---|---|---|
| Anthropic | `POST /anthropic/v1/messages` (alias `POST /v1/messages`) | `messages` | `messages` | `<gateway>/anthropic` |
| Anthropic | `POST /anthropic/v1/messages/count_tokens` (alias `POST /v1/messages/count_tokens`) | `count_tokens` | `count_tokens` | `<gateway>/anthropic` |
| OpenAI | `POST /openai/v1/responses` | `responses` | `responses` | `<gateway>/openai/v1` |
| OpenAI | `POST /openai/v1/chat/completions` | `openai_chat` | `chat_completions` | `<gateway>/openai/v1` |
| OpenAI | `POST /openai/v1/embeddings` | `embeddings` | `embeddings` | `<gateway>/openai/v1` |
| DeepSeek | `POST /deepseek/v1/chat/completions` | `deepseek_chat` | `chat_completions` | `<gateway>/deepseek/v1` |
| TypeSafe | `POST /typesafe/v1/systemone` | `systemone` | `systemone` | `<gateway>/typesafe` (`TYPESAFE_BASE_URL`) |

Routes exist only for providers listed in the config. Query strings are rejected, with one
exception: the Anthropic routes accept `?beta=true`, which the Anthropic SDK beta client adds. The
gateway removes it before forwarding the request.

Per-API request rules worth knowing. For OpenAI, DeepSeek and TypeSafe, any request field the
gateway does not recognize is rejected with `unsupported_option`. Anthropic is more permissive (see
below).

- **Anthropic:** an `anthropic-version: 2023-06-01` header is required. Every `anthropic-beta` value
  must be listed in the profile's `approved_beta`. See [Anthropic pass-through policy](#anthropic-pass-through-policy).
- **OpenAI Responses:** `store: false` and `max_output_tokens` are required. Background mode,
  stored state, file/item references and server tools are rejected.
- **OpenAI Chat:** the profile defines which output limit field is required (`max_tokens` or
  `max_completion_tokens`). Streaming requires `stream_options.include_usage: true`. `n` must be 1.
- **Embeddings:** the input is a string or 1–256 strings. Dimensions and encodings come from the profile.
- **DeepSeek Chat:** `max_tokens` is required. Images are not supported.
- **TypeSafe System One:** a request has `model`, `state` and `questions`, and each question is
  `noul`, `choice` or `score`. Images are not supported: `assets`, `box` questions and the
  `TypeSafe-Danger-Zone-Images` header are rejected. Long context is not supported either: the
  `TypeSafe-Danger-Zone-Long-Context` header is rejected. The gateway accepts a request body
  compressed with zstd (`Content-Encoding: zstd`), which the TypeSafe SDKs send by default for
  bodies of 1 KiB or more. The compression window may be at most 8 MiB, and the decompressed body
  must fit `limits.inference_body_bytes` and the profile's `max_body_bytes`. Other encodings get
  `415 unsupported_media_type`. `GET /v1/models` is not served.
- Images: only inline base64 PNG is accepted, and only on profiles with the `inline_png` feature.
  JPEG, URLs, files and documents (PDF) are rejected.

Error responses generated by the gateway use each provider's native error envelope (for TypeSafe,
`{"detail": "Gateway request could not be completed: gateway_<code>"}`). The specific reason is in the
`x-gateway-error-code` response header (for example `unsupported_option`, `unsupported_model`,
`budget_exhausted`, `rate_limited`, `unsettled_usage_block`, `inference_paused`,
`provider_credential_not_authorized`, `provider_credential_unbound`, `virtual_key_required`,
`adapter_failed`). If the gateway knows the
request never reached the provider, the error carries `x-gateway-dispatch-state: not_dispatched`.
Any other gateway error carries `x-should-retry: false` instead. Every response has an
`x-gateway-request-id`.

## Quick start

### 1. Prerequisites

- **PostgreSQL** reachable over TLS. Plaintext connections are never made. By default the gateway
  verifies the certificate chain and host name (`verify-full`). If the server certificate is not issued
  by a publicly trusted CA, point `TLS_CA_FILE` at a PEM bundle, or set `database.ssl_mode: require`
  to encrypt without verifying the certificate (for example, inside a private network where shipping
  the CA is impractical). Do not set any `PG*` environment variables (`PGHOST`, `PGPASSWORD`,
  `PGSSLMODE`, …). If one is present, even empty, the gateway refuses to connect.
- In `sso` mode, **an SSO** that issues RS256 access JWTs and serves a JWKS. See [Authentication](#authentication). In `password` mode, an SMTP server instead (see [Password mode](#password-mode)).
- **A TLS-terminating reverse proxy.** The gateway serves plain HTTP on `0.0.0.0:4000`, but all
  configured URLs must be `https://`.
- Provider API keys for the providers you enable.

Migrations in `migrations/` are compiled into the binary. They run automatically at startup under a
Postgres advisory lock, and each one's checksum is verified. Startup fails if the database has an
unknown or modified migration.

### 2. Configuration

```sh
cp config.example.yaml gateway.yaml
$EDITOR gateway.yaml        # hosts, e-mails, database, profiles and, above all, rates
```

### 3. Secrets (environment only)

```sh
export DB_PASSWORD='...'
export CSRF_SIGNING_KEY="$(openssl rand -hex 32)"           # exactly 64 lowercase hex characters
export CREDENTIAL_ENCRYPTION_KEY="$(openssl rand -hex 32)"  # different value; keep it stable
export ANTHROPIC_ADMIN_API_KEY='sk-ant-admin01-...'         # optional: token check and correction
```

Provider API keys are **not** environment variables. An admin registers them in the dashboard after
the first start (see [Provider keys](#provider-keys)). If `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` or
`DEEPSEEK_API_KEY` is set, the gateway refuses to start with `config_provider_key_env_removed`, so
that a key you meant to use is not silently ignored.

### 4. Run

From source (Rust 1.95, pinned in `rust-toolchain.toml`; Node 22.22.2 for the dashboard):

```sh
(cd web && npm ci && npm run build)       # produces web/dist, required at startup
cargo build --locked --release
./target/release/m-gated --config gateway.yaml
```

The dashboard bundle is loaded from `web/dist` relative to the working directory, so run the binary
from the repository root (or a directory that contains `web/dist`).

With Docker (multi-stage build, non-root user `65532`, `STOPSIGNAL SIGTERM`). Released images for
`linux/amd64` and `linux/arm64` are published to `ghcr.io/bldg-7/m-gated`:

```sh
docker run --rm -p 4000:4000 \
  -v "$PWD/gateway.yaml:/app/gateway.yaml:ro" \
  -e DB_PASSWORD -e CSRF_SIGNING_KEY -e CREDENTIAL_ENCRYPTION_KEY \
  ghcr.io/bldg-7/m-gated:0.5.0
```

Or build it yourself with `docker build -t m-gated .` and run `m-gated` in place of the image name.

The image's working directory is `/app`, so it reads `/app/gateway.yaml` by default. You can also
pass `--config <path>`.

### 5. First use

1. The fixed admin (the person named by `admin_email`) opens `https://<gateway>/dashboard/` and
   signs in. In `sso` mode this is the SSO sign-in. In `password` mode the fixed admin first sets a
   password with the setup link that the gateway writes to its start log (see
   [Password mode](#password-mode)). Under *Admin → Access list*, the admin adds the people who may use the
   gateway, and chooses who else is an admin (see [Access list](#access-list)). Under *Admin → Provider keys* (`/dashboard/admin/credentials`), the admin registers a provider key
   for each configured provider. Without one, `/readyz` still returns 200 so the admin can reach the
   dashboard, but no request can go out to that provider.
2. New principals start **deny-all**. Under *Admin → Principals*, the admin sets a policy for each
   user, including the admin's own account. A policy has grants (provider, model, capabilities) and
   optional limits: a monthly budget, `rpm`, `tpm` and `count_rpm`. On the same user's page, the
   admin also authorizes the provider keys that user may use (**Provider key access**).
3. Users issue a virtual key on the *Virtual keys* screen, choosing one of their authorized provider
   keys, and point their SDKs at the gateway with that virtual key. Inference requests are accepted
   only with virtual keys.

A virtual key can be issued only when all three of these conditions hold. The dashboard checks
them in this order and shows the first one that fails:

1. **The policy form has models to choose from.** The choices are the profiles in the config plus
   the approved [model catalog](#model--price-catalog). On a new installation that uses `catalog`,
   the list is empty until an admin syncs and approves the catalog under *Admin → Models & prices*.
2. **The owner's policy has at least one grant.** A new user and a new service principal both start
   with no grants.
3. **The owner is authorized for a provider key of each provider in the grants.** An admin gives
   this authorization on the owner's page (**Provider key access**).

## Command line

```
m-gated [--config <path>] [--env-file <path>] [--maintenance]
```

| Flag | Meaning |
|---|---|
| `--config <path>` | YAML config (default `gateway.yaml`, max 256 KiB). |
| `--env-file <path>` | Optional `KEY=VALUE` file of **non-secret** overrides (max 64 KiB). Only the non-secret keys from the table below are allowed. Secrets, unknown keys, duplicates and values containing quotes, `$` or backticks are rejected. |
| `--maintenance` | Runs a single admin command read from stdin and exits. No HTTP listener. See [Maintenance mode](#maintenance-mode). |

If startup validation fails, the gateway writes a fixed error code to stderr and exits with status 1
before binding the port. The codes include `config_input_invalid`, `config_syntax_invalid`,
`config_auth_invalid`, `config_url_invalid`, `config_identity_invalid`, `config_secret_missing`,
`config_secret_invalid`, `config_database_invalid`, `config_profile_invalid`, `config_price_invalid`,
`config_limits_invalid`, `config_provider_key_env_removed`, `ui_bundle_unavailable` and
`schema_unavailable`.

## Configuration reference

The configuration file is strict. Unknown keys are rejected, and so are YAML anchors, aliases,
tags and merge keys. [`config.example.yaml`](config.example.yaml) is an annotated example.

### Top-level keys

| Key | Required | Description |
|---|---|---|
| `public_origin` | yes | HTTPS origin of the gateway (no path or port). Cookie-authenticated writes must send exactly this `Origin`. |
| `auth` | yes | `mode` (`sso` or `password`, required) and the SSO settings, see below. |
| `smtp` | in `password` mode | The SMTP server for invitations and reset links, see [Password mode](#password-mode). Refused in `sso` mode. |
| `admin_email` | yes | The fixed admin: one e-mail address. The gateway lowercases it. At each start, the gateway adds it to the [access list](#access-list) as an admin. |
| `database` | yes | `host`, `port`, `name`, `user`, and optional `ssl_mode` (`verify-full` by default, or `require`). The password comes from `DB_PASSWORD`. |
| `providers` | yes | 1–4 entries, at most one per provider. |
| `limits` | – | Process-wide concurrency, size and timeout limits. |
| `retention` | – | Retention period for ledger, audit and command records. |
| `execution_ceiling` | – | Server-wide ceiling that applies to every policy. |

### `auth`

`mode` is required: `sso` or `password` (environment variable `AUTH_MODE`). It has no default.
The keys below are the SSO settings. They are required in `sso` mode. In `password` mode they are
refused: if any of them, or its environment variable, is set, the gateway does not start. The error
names the value, for example `config_value_refused: AUTH_ISSUER`. A missing required value gives
`config_value_missing: <name>` in the same way.

| Key | Description |
|---|---|
| `issuer` | Must equal the JWT `iss` claim exactly, in canonical form: lowercase host, no path, **no trailing slash**. |
| `audience` | Must equal the JWT `aud` claim (a string, or an array with exactly one element). It must be an HTTPS origin with no path and no trailing slash. |
| `jwks_uri` | JWKS endpoint. Keys are cached for 300 s. |
| `access_cookie_name` | Name of the cookie in which the SSO stores the access JWT for the browser. |
| `login_url`, `refresh_url`, `logout_url` | SSO endpoints used by the dashboard. |
| `return_param` | Optional. Name of the query parameter your SSO reads to send the user back after login or logout (for example `return_to`). When set, the gateway sends users to `login_url?<return_param>=<public_origin>/dashboard/` and `logout_url?<return_param>=<public_origin>/dashboard/`. Without it, the SSO decides where users land after login. Letters, digits, `_` and `-` only. A `return_to` query sent by a client to `/auth/login` is always ignored. |

`jwks_uri`, `login_url`, `refresh_url` and `logout_url` must have the **same origin as `issuer`**,
because the dashboard's Content-Security-Policy allows exactly one SSO origin. URLs must use
`https`, must not include an explicit port, userinfo, query or fragment, and must be written in
canonical form.

### `providers` and profiles

```yaml
providers:
  - provider: anthropic            # anthropic | openai | deepseek | typesafe
    base_url: https://api.anthropic.com   # must be the official origin
    profiles: [ ... ]              # 1-64 profiles, (model, api) unique
```

Instead of `profiles`, an Anthropic or OpenAI provider can take its models and prices from the
database-backed [model catalog](#model--price-catalog). The config then holds only the policy that
applies to every catalog model of that provider:

```yaml
  - provider: anthropic
    base_url: https://api.anthropic.com
    catalog:
      api: messages                  # anthropic: messages; openai: responses | openai_chat
      fixture_version: anthropic-messages-v1
      features: [client_tools, thinking, cache, json_schema, inline_png]
      approved_beta: [ ... ]         # same meaning as in a profile
      max_body_bytes: 8388608
      max_response_bytes: 16777216
      model_prefixes: [claude-]      # which models a sync offers as new candidates
```

A provider has either `profiles` or `catalog`, not both.

`base_url` must be exactly `https://api.anthropic.com`, `https://api.openai.com`,
`https://api.deepseek.com` or `https://api.typesafe.ai`. Outbound requests ignore proxy environment variables, do not follow
redirects and are never retried by the gateway.

As a trial, the DeepSeek and OpenAI providers can run their adapters as WebAssembly (WASM)
components instead of the built-in code ([decision 0009](docs/decisions/0009-deepseek-adapter-as-wasm-trial.md),
[decision 0013](docs/decisions/0013-openai-adapter-as-wasm.md)). The OpenAI component serves all
three OpenAI APIs: `responses`, `openai_chat` and `embeddings`. Add both fields to the provider:

```yaml
  - provider: deepseek
    base_url: https://api.deepseek.com
    adapter: /app/adapters/deepseek.wasm     # in the release image
    adapter_sha256: <64 lowercase hex digits>
  - provider: openai
    base_url: https://api.openai.com
    adapter: /app/adapters/openai.wasm       # in the release image
    adapter_sha256: <64 lowercase hex digits>
```

The gateway does not start if the file's SHA-256 differs, if the component imports anything, if it
reports a provider name or API that the built-in adapter does not have. It also does not start if
the component reports other values than the built-in adapter for any of these three facts:

- the usage categories of an API;
- whether an API can use the model catalog (`catalog`);
- the provider's name in the models.dev price list (`models-dev-name`).

The catalog, its sync and the dashboard know only the built-in values today, so the values must be
equal. The component must export version 0.3.0 of the adapter interface, under the name
`m-gated:adapter/provider@0.3.0` ([decision 0013](docs/decisions/0013-openai-adapter-as-wasm.md)).
A component built for an earlier version (0.1.0 or 0.2.0) is refused at start. Each call gets a new
sandbox with a 128 MiB memory limit and a CPU limit. If the component fails while it checks a
request, the gateway refuses the request with `500` and `x-gateway-error-code: adapter_failed`, and
does not send it. If it fails while it reads a response, the response is malformed and the request
becomes `unknown`. One process uses one implementation per provider, so every configuration that a
process loads must name the same component for that provider.

Release images contain both components:

The image labels let you read a component's SHA-256 from the image metadata (`docker inspect`)
without starting a container.

| Provider | Component | SHA-256 file | Image label |
|---|---|---|---|
| DeepSeek | `/app/adapters/deepseek.wasm` | `/app/adapters/deepseek.wasm.sha256` | `m-gated.adapters.deepseek.sha256` |
| OpenAI | `/app/adapters/openai.wasm` | `/app/adapters/openai.wasm.sha256` | `m-gated.adapters.openai.sha256` |

The gateway uses a component only when the config names it. Without `adapter`, the built-in
adapter runs. To get the value for `adapter_sha256`:

```sh
docker run --rm --entrypoint cat <image> /app/adapters/deepseek.wasm.sha256
docker run --rm --entrypoint cat <image> /app/adapters/openai.wasm.sha256
```

The release workflow builds the components once, on Linux amd64, and puts the same files in the
amd64 and arm64 images. So one `adapter_sha256` per component fits both. A build on another kind of machine
(for example macOS, or Linux arm64) gives a different file with a different SHA-256, because Cargo
puts the kind of machine into the names that it gives each crate.

To use a component of your own, give its path and SHA-256 in the same two fields. Put the file in
the container with a volume, or build an image `FROM` the release image that adds the file.

| Profile field | Description |
|---|---|
| `model` | Model name that clients send (≤ 200 bytes). |
| `api` | `messages`, `count_tokens` (Anthropic); `responses`, `openai_chat`, `embeddings` (OpenAI); `deepseek_chat` (DeepSeek); `systemone` (TypeSafe). |
| `profile_version`, `pricing_version`, `fixture_version` | Operator-chosen version labels (≤ 64 bytes) recorded in each request's ledger snapshot. They describe the profile, its rates, and the response/usage format the profile was checked against. Change `pricing_version` whenever you change rates. |
| `actual_models` | Model names the provider may report in its response. Must include `model`. If the response reports any other model, the request becomes `unknown`. When a TypeSafe request names an alias such as `jev-latest`, the response names the model that the alias points to (for example `jev-1.14.0`), so list both. |
| `features` | Subset of `client_tools`, `thinking`, `cache`, `json_schema`, `inline_png`. A request that uses a feature the profile does not list is rejected. TypeSafe profiles take no features. |
| `approved_beta` | Anthropic only. The `anthropic-beta` values that clients may send. |
| `max_input_tokens` | Worst-case billable input tokens per request (including cache reads and writes). Set it to the model's full context window. |
| `max_output_tokens` | Largest output limit a request may ask for. Required for generating APIs, not allowed for `count_tokens`, `embeddings` and `systemone`. |
| `max_body_bytes` | Request size limit (≤ `limits.inference_body_bytes`, max 8 MiB). |
| `max_response_bytes` | Response size limit (≤ 16 MiB; ≤ 256 KiB for `count_tokens`). |
| `rates` | Price per category, see below. |
| `billing_class` | `chargeable` (default) or `guaranteed_free`. A `guaranteed_free` profile is exempt from budgets and unknown-cost blocks. It is allowed only for Anthropic `count_tokens`, and all its rates must be zero. |
| `count_billing` | Required on a chargeable `count_tokens` profile. The only value is `counted_input_tokens`, which bills the counted tokens at the `input` rate. |
| `chat_output_limit_field` | `openai_chat` only, and required: `max_tokens` or `max_completion_tokens`. `max_tokens` cannot be combined with `thinking`. |
| `embedding_dimensions`, `embedding_encodings` | `embeddings` only, and required: `{default, max ≤ 4096}` and a subset of `[float, base64]`. Embedding profiles take no `features`. |

**Rates.** Rates are **USD per 1,000,000 tokens**. Each one is a quoted fixed-point string with
exactly 12 decimal places, for example `'3.000000000000'`. The gateway splits usage and rates into
usage categories, so that each kind of token, such as a cache write that lives one hour, is priced
at its own rate ([decision 0012](docs/decisions/0012-usage-categories.md)). The gateway has one list
of usage categories, and they are also the price categories:

| Category | Tokens |
|---|---|
| `input` | Input that was not read from the cache |
| `output` | Output. Reasoning (thinking) tokens are part of it and are not priced again |
| `cache_read` | Input read from the cache |
| `cache_write` | A cache write whose price does not depend on how long the cache lives. No built-in API uses it now |
| `cache_write_5m` | A cache write that lives five minutes |
| `cache_write_1h` | A cache write that lives one hour |

Each API declares the categories that its usage can have, and a profile can set rates only for
those categories:

| API | Categories |
|---|---|
| Anthropic `messages` | `input`, `output`, `cache_read`, `cache_write_5m`, `cache_write_1h` |
| Anthropic `count_tokens`, OpenAI `embeddings` | `input` |
| OpenAI `responses` and `openai_chat`, DeepSeek `deepseek_chat` | `input`, `output`, `cache_read` |
| TypeSafe `systemone` | `input`, `output` |

Which rates are required depends on the profile:

- Every profile needs `input`. Generating APIs also need `output`.
- `embeddings`: `input` only.
- DeepSeek: exactly `input`, `output` and `cache_read`, with or without the `cache` feature.
- TypeSafe: exactly `input` and `output`, and `output` must be `'0.000000000000'`. TypeSafe does
  not charge for output tokens, and a request has no output limit that could bound an output cost.
- OpenAI with the `cache` feature: `cache_read`.
- Anthropic `messages` with the `cache` feature: `cache_read` and `cache_write_5m`.
  `cache_write_1h` is optional. Without it, a request that asks for the one-hour cache is refused
  (see [Anthropic pass-through policy](#anthropic-pass-through-policy)).

`config.example.yaml` ships with every rate set to `'0.000000000000'` as a placeholder. Zero is a
valid rate, so if you leave the placeholders in place, every request settles at 0 USD and budgets
never trigger. Take every rate from the provider's current price list for that
exact model.

### Environment variables

Environment values take precedence over `--env-file`, and `--env-file` takes precedence over YAML.
The gateway reads only the variables in this table.

| Variable | Overrides | Notes |
|---|---|---|
| `PUBLIC_ORIGIN` | `public_origin` | |
| `AUTH_ISSUER`, `AUTH_JWKS_URI`, `SSO_AUDIENCE`, `SSO_ACCESS_COOKIE_NAME` | `auth.issuer`, `auth.jwks_uri`, `auth.audience`, `auth.access_cookie_name` | |
| `SSO_LOGIN_URL`, `SSO_REFRESH_URL`, `SSO_LOGOUT_URL` | `auth.login_url`, `auth.refresh_url`, `auth.logout_url` | |
| `SSO_RETURN_PARAM` | `auth.return_param` | |
| `ADMIN_EMAIL` | `admin_email` | |
| `AUTH_MODE` | `auth.mode` | `sso` or `password`. |
| `SMTP_HOST`, `SMTP_PORT`, `SMTP_USERNAME`, `SMTP_FROM` | `smtp.*` | `password` mode only. |
| `SMTP_PASSWORD` | – | Secret, environment only. `password` mode only. |
| `DB_HOST`, `DB_PORT`, `DB_NAME`, `DB_USER`, `DB_SSL_MODE` | `database.*` | `DB_SSL_MODE` accepts only `verify-full` or `require`. |
| `TLS_CA_FILE` | – | Path to an extra PEM trust anchor (≤ 64 KiB) for Postgres, the JWKS and provider connections. |
| `DB_PASSWORD` | – | **Secret, environment only.** |
| `CSRF_SIGNING_KEY` | – | **Secret, environment only.** 64 lowercase hex characters (32 bytes). It signs CSRF tokens and pagination cursors. |
| `CREDENTIAL_ENCRYPTION_KEY` | – | **Secret, environment only.** 64 lowercase hex characters (32 bytes), different from `CSRF_SIGNING_KEY`. It encrypts the provider keys stored in Postgres. If it changes, stored keys can no longer be decrypted and must be registered again. |
| `ANTHROPIC_ADMIN_API_KEY` | – | **Secret, environment only. Optional.** An Anthropic Admin API key (`sk-ant-admin01-…`) of the organization that owns the Anthropic provider keys. It turns on the [token check and correction](#token-check-against-the-provider-optional). |

### `limits`

All fields are optional. Each default is also the **maximum**: you can only lower a limit. The
gateway also checks that limits are consistent with each other, for example
`per_principal ≤ inference ≤ http_requests` and
`transport_timeout_ms + finalization_timeout_ms ≤ request_timeout_ms`.

| Key | Default | Key | Default |
|---|---|---|---|
| `connections` | 64 | `header_bytes` | 32768 |
| `http2_streams` | 16 | `header_count` | 64 |
| `http_requests` | 40 | `token_bytes` | 8192 |
| `inference` | 16 | `header_timeout_ms` | 5000 |
| `per_principal` | 4 | `inference_body_bytes` | 8388608 |
| `pre_auth` | 4 | `management_body_bytes` | 65536 |
| `authentication` | 4 | `body_timeout_ms` | 30000 |
| `nonstream` | 4 | `body_idle_timeout_ms` | 5000 |
| `management` | 4 | `management_timeout_ms` | 10000 |
| `management_writes` | 1 | `asset_timeout_ms` | 30000 |
| `assets` | 8 | `request_timeout_ms` | 900000 |
| `health_interval_ms` | 5000 | `transport_timeout_ms` | 895000 |
| `health_valid_ms` | 10000 | `finalization_timeout_ms` | 5000 |

When a concurrency limit is full, new requests are rejected immediately (HTTP 429 or 503). They are
not queued.

### `retention` and `execution_ceiling`

`retention.ledger`, `retention.audit` and `retention.commands` each accept `3_months`, `6_months`,
`1_year`, `3_years` or `unlimited`. If a key is omitted, nothing is deleted. Choosing a period never
deletes data by itself. Per-request detail that is older than the period and fully settled is
removed only by the explicit maintenance command. Per-month totals are kept, so usage sums for
those months stay correct.

`execution_ceiling` has the same shape as a policy's limits:
`{budget: {limit_usd, period: {kind: calendar_month}}, rpm, tpm, count_rpm}`.
For every principal and key, the effective limit is the smaller of the policy and the ceiling.
The ceiling has no time zone. If you add `timezone` to it, startup fails.
See [Budget time zone](#budget-time-zone).

A virtual key's own limits cannot be higher than this effective limit of its owner. `GET /api/me`
and `GET /api/admin/principals/{id}` return it as `key_limit_maximum` (the same shape as limits; a
`null` field has no maximum). Issuing a key with a budget, `rpm`, `tpm` or `count_rpm` above it is
rejected with `422 limit_exceeds_maximum`.

### Budget time zone

Every monthly budget uses one time zone: the budgets of user policies, of virtual keys and of the
execution ceiling. A budget period is a calendar month in that time zone, and spend resets to 0 at
00:00 on the 1st. Admins set the time zone on the dashboard's **Settings** screen
(`/dashboard/admin/settings`). Policies and keys do not have their own time zone, and the API rejects
a `timezone` field in their limits with `400 invalid_request`.

A change does not apply at once. The current period keeps the old time zone until it ends. The new
time zone starts with the next period. The first period after the change is the only exception: it
starts when the previous period ends and ends at the first 00:00 on the 1st of a month (in the new
time zone) that is more than two days later. So it can be a few hours shorter or longer than a
calendar month. For example, if you
change from `UTC` to `Asia/Seoul` in November 2030, the November period ends as before at
2030-12-01 00:00 UTC (09:00 in Seoul). The December period then runs to 2031-01-01 00:00 in Seoul,
9 hours shorter than usual. From January on, periods start at 00:00 Seoul time. Until the next
period starts, you can change the setting again. Choosing the time zone that is in use now cancels
the scheduled change.

API (admin only; writes need `X-Gateway-Command-Id`, and cookie writes need `Origin` and `X-CSRF-Token`):

| Method and path | Body | Result |
|---|---|---|
| `GET /api/admin/settings` | – | `budget_timezone`: the current time zone and period, the scheduled change (`pending`, or `null`) and `version`. `cutoff_rule`: how cut-off Anthropic streams are settled (`received` or `max_tokens`), whether correction is on (`correction`), whether it can be turned on on this server (`correction_available`) and `version` |
| `PUT /api/admin/settings/cutoff-rule` | `{"provider": "anthropic", "cutoff_rule": "received" \| "max_tokens", "correction": true \| false, "expected_version": "<n>"}`. `correction` is optional; without it, the switch keeps its value | `200`; `409 version_conflict` if another admin changed it since you read `version`; `400` for `"correction": true` when `ANTHROPIC_ADMIN_API_KEY` is not set. Requests admitted after the change use the new rule |
| `PUT /api/admin/settings/budget-timezone` | `{"timezone": "<IANA name>", "expected_version": "<n>"}` | `200`; `409 version_conflict` if another admin changed it since you read `version`; `400` for an unknown time zone |

When you upgrade to 0.9.0, the setting starts with the time zone that most budgets used before
(`UTC` if none had a budget). Budgets that used that time zone keep this month's spend. A budget
that used a different time zone starts a new period at the upgrade, so its spend for this month
starts again from 0.

## Authentication

**SSO JWTs.** The gateway checks each token as follows:

- `alg` is `RS256` and `kid` is present.
- The signature verifies against the configured JWKS. Token-supplied `jku`, `x5u` and `jwk` headers
  are ignored.
- `iss` and `aud` match the config exactly.
- `exp` is set, and `exp` and `nbf` hold with 30 s of leeway.
- `sub` is a non-empty string.
- `email_verified` is `true`, and `email`, lowercased, is on the [access list](#access-list).
- An optional `name` is used as the display name.

A person is identified by the pair (`iss`, `sub`). The e-mail address is used only to look up the
access list. The gateway never issues or refreshes tokens and never stores
refresh tokens.

| Credential | Inference | Own data and keys (`/api/*`) | Admin (`/api/admin/*`) |
|---|---|---|---|
| SSO JWT in `Authorization: Bearer` or `x-api-key` | **no** (`403 virtual_key_required`) | yes | if admin |
| SSO JWT in the access cookie (browser) | **no** | yes (writes need CSRF) | if admin |
| Virtual key (starts with `mgw_`) | yes, within the key's grants and limits | no | no |

In `password` mode, the session token from the `mgated_session` cookie (or the same token in
`Authorization: Bearer`) takes the place of the SSO JWT in this table. See [Password mode](#password-mode).

If a request sends both `Authorization` and `x-api-key`, the two values must be identical. Claude
Code sends both.

**Browser flow.** `/auth/login` redirects to `login_url` when the request has no valid cookie.
After sign-in, the SSO must set `access_cookie_name` on a domain that includes the gateway host and
send the user back to `/dashboard/`. The dashboard calls `refresh_url` with
`POST` and `credentials: 'include'`, so the SSO must allow that CORS request from `public_origin`.
For logout, the dashboard navigates to `logout_url`. Cookie-authenticated writes require
`Origin == public_origin` and an `X-CSRF-Token` obtained from `GET /api/csrf`.

**Virtual keys.** Users issue virtual keys on the dashboard (`POST /api/keys`). A virtual key has:

- a name;
- a validity period (`3_months`, `6_months`, `1_year`, `3_years` or `unlimited`);
- one or more grants (provider, model, capabilities). They must be a subset of the owner's current
  grants;
- for each provider in its grants, one provider key the owner is authorized to use (see
  [Provider keys](#provider-keys)). A virtual key is therefore always bound to at least one provider
  key;
- optional limits.

**Editing a virtual key.** After a virtual key is issued, its owner or an admin can change three
things: the name, the grants, and the provider key that the virtual key uses for each provider in
its grants. Use `PATCH /api/keys/{key id}`, or the **Edit** button in the key list. The validity
period, the expiry time and the limits cannot be changed. To change them, revoke the key and issue a
new one. A revoked or expired key cannot be edited.

- The request body must contain all four fields. The gateway replaces the name, the grants and the
  provider keys with the values in the request, also when only one of them is different:
  `{"expected_version": "<n>", "name": "...", "grants": [...], "provider_credentials": {"anthropic": "<credential id>"}}`.
- `expected_version` is the `version` field of the key, as a string, from the key list
  (`GET /api/keys`). The version goes up each time the key is edited or revoked. If it is not the
  current version, the gateway answers `409 version_conflict`.
- The rules are the same as for issue:
  - The grants of the virtual key must be a subset of the grants in the owner's current policy.
  - At least one grant is required.
  - For each provider that appears in the grants, `provider_credentials` must name one provider key
    that the owner is authorized to use.
- A change applies to the next request that uses the virtual key.
- The audit log records the name, the grants and the provider keys before and after each edit.

The key is shown once. Only a hash is stored. Each
principal can have up to 32 active keys. Admins can also create **service principals**, which have a
human owner, and issue keys for them.

Every management write (`POST`, `PUT`, `PATCH`, `DELETE`) requires an `X-Gateway-Command-ID`
header containing a UUID. Repeating a command with the same ID and the same input replays the
original result, so retrying is safe.

### Password mode

With `auth.mode: password`, the gateway is its own sign-in service. There is no SSO. People sign in
with an e-mail address and a password, and an admin invites them by e-mail
([decision 0011](docs/decisions/0011-password-sign-in.md)).

```yaml
auth:
  mode: password
smtp:
  host: smtp.example.com
  port: 587            # 465: TLS from the start. Any other port: STARTTLS is required.
  username: gateway
  from: gateway@example.com
# SMTP_PASSWORD comes from the environment only.
```

- **Principals.** The gateway finds a person's principal by an issuer and a subject. In `password`
  mode the issuer is `public_origin` and the subject is the lower-case e-mail address. The principal
  is made at the first sign-in, right after the person sets a password.
- **Passwords.** 12 to 128 characters, with no rules about character kinds. Only an argon2id hash is
  stored (19 MiB of memory, 2 passes, no parallel threads). Only the person can set or change their own password.
- **Links.** Three kinds of one-use link point to `/dashboard/password#token=<token>`. The token is
  in the URL fragment, so it does not reach server logs or the `Referer` header, and only its
  SHA-256 is stored.
  - An invitation is valid for 7 days. A reset link and a setup link are valid for 24 hours.
  - When a person sets a password, all open links of that person stop working, and so do all of
    that person's sessions.
  - When an admin sends a new invitation or reset link, the earlier invitation and reset links of
    that person stop working.
- **Mail.** The gateway sends invitations and reset links through the SMTP server, over TLS. It
  stores the link first and then sends the mail. If the server refuses the mail or does not answer
  in 10 seconds, the gateway removes the link.
- **The first admin.** While the fixed admin (`admin_email`) has no password, the gateway writes a
  setup link to stderr at each start, as a JSON line with `"warning": "first_admin_setup_link"`.
  Each start makes a new setup link, and earlier setup links stay valid until they expire or the
  password is set. Anyone who can read the start log can use the link, so limit log access to
  operators. If no admin can sign in, change `admin_email` to an address that has no password yet,
  and restart.
- **Sessions.** Signing in sets the cookie `mgated_session` (`HttpOnly`, `Secure`, `SameSite=Lax`)
  with a random token. Only the token's SHA-256 is stored. A session ends in these cases, and the
  gateway checks them on each request:
  - 12 hours after sign-in;
  - at sign-out;
  - when the person changes or resets the password;
  - when the person is suspended, or removed from the access list.

  Cookie-authenticated writes need `Origin` and a CSRF token, as in `sso` mode.
- **Failed sign-ins.** A wrong password and an unknown address get the same answer
  (`401 invalid_credentials`) and take the same time. After 5 failures for one address, that address
  cannot sign in for 15 minutes (`429 sign_in_locked` with `Retry-After`). Password hash checks count
  toward `limits.authentication`, the number of authentication checks that run at the same time.
- **Dashboard.** `/auth/login` sends a person who is not signed in to `/dashboard/signin`. The
  sign-in screen has no "forgot password": an admin sends a reset link. Each person can change their
  own password from the sidebar.

| Method and path | Body | Result |
|---|---|---|
| `POST /auth/password` | `{"email": "...", "password": "..."}` | `204` and the session cookie; `401 invalid_credentials`; `429 sign_in_locked`; `403 access_denied` if the account is suspended |
| `POST /auth/password/link` | `{"token": "..."}` | `200` with `email`, `kind` (`invitation`, `reset` or `setup`), `password_set` and `expires_at`; `404 link_invalid` |
| `POST /auth/password/set` | `{"token": "...", "password": "...", "display_name"?: "..."}` | `204` and a session cookie (the person is signed in). `display_name` is required when `password_set` is `false`. `400 password_invalid`; `404 link_invalid` |
| `POST /auth/logout` | – | `204`; the session ends and the cookie is cleared |
| `POST /api/me/password` | `{"current_password": "...", "new_password": "..."}` | `204`; all sessions of the person end. `400 current_password_invalid` or `password_invalid` |
| `POST /api/admin/access/{id}/link` | empty; needs `X-Gateway-Command-ID` | `200`; sends an invitation to a person without a password, or a reset link to a person with one. `502 mail_failed` if the mail could not be sent. A failed send is not recorded, so sending the same command ID again tries again; after a success, the same command ID returns the first result and sends no mail |

These routes exist only in `password` mode. `POST /api/me/password` does not use the command ID.

### Access list

The access list in the database decides who can use the gateway and who is an admin. Each entry of
the list is an e-mail address and a role (`user` or `admin`). Because the list is keyed by e-mail
address, an admin can add a person before that person signs in for the first time. The person's
principal is made at the first sign-in.

- The config names one **fixed admin** (`admin_email`). At each start, the gateway adds this address
  to the list as an admin. If the address is already on the list as a user, its role becomes
  `admin`. The fixed admin's entry cannot be changed or removed on the dashboard.
- Admins add, change and remove other people under *Admin → Access list*
  (`/dashboard/admin/members`). An admin cannot change or remove their own entry.
- In `password` mode, adding a person also sends an invitation. The answer says whether the mail
  went out (`"invitation": "sent"` or `"failed"`). Removing a person also removes their password,
  links and sessions, so a person who is added again needs a new invitation.
- A change applies to the next request. A person who is removed can no longer sign in. Their virtual
  keys stop working (`403 access_denied`), and so do the keys of the service principals that they
  own (`403 owner_inactive`). Their principal, virtual keys and usage stay in the database.
- Removing a person ends their access. They get it back only if an admin adds them again. To block
  a person for a while and keep their entry, suspend their principal instead (see **Suspending an
  account** under [Operations](#operations)).
- Every change is written to the audit log.
- If no admin can sign in, change `admin_email` and restart. In `sso` mode, choose a person who can
  sign in through the SSO. In `password` mode, choose an address that has no password yet; the
  gateway then writes a setup link for it (see [Password mode](#password-mode)). That person
  becomes the fixed admin. The earlier fixed admin stays on the list as a normal admin and can then
  be removed on the dashboard.

API (admin only; writes need `X-Gateway-Command-ID`, and cookie writes need `Origin` and `X-CSRF-Token`):

| Method and path | Body / query | Result |
|---|---|---|
| `GET /api/admin/access` | – | `items`, sorted by e-mail address. Each entry has `id`, `email`, `role` (`user` or `admin`), `fixed` (the fixed admin), `added_by_email` (`null` for the fixed admin), `added_at`, `version`, `principal`: once the person has signed in, the principal's `id` and `disabled` (`true` while the principal is suspended), else `null`, and `password_set` (in `password` mode, whether the person has set a password; always `false` in `sso` mode) |
| `POST /api/admin/access` | `{"email": "...", "role": "user" \| "admin"}` | `201`; `409 access_exists` if the address is already on the list |
| `PATCH /api/admin/access/{id}` | `{"role": "user" \| "admin", "expected_version": "<n>"}` | `200`; `409 version_conflict` if the entry changed since you read `version`; `409 access_protected` for the fixed admin's entry or your own entry |
| `DELETE /api/admin/access/{id}` | `?expected_version=<n>` | `200`; the same conflicts as `PATCH` |

## Cost ledger

Every inference request goes through the ledger in Postgres:

1. **Admission.** The gateway checks the request against the principal's policy, the key's policy
   (if any) and the execution ceiling: grants, rate windows (fixed 60 s), budget, pause state and
   outstanding unknowns. It also computes a worst-case cost bound:
   `max_input_tokens × (highest non-output rate) + requested output limit × output rate`.
2. **Dispatch.** The request is forwarded once. A budget allows paid calls while the settled spend
   for the current budget period is below the limit. Nothing is held for calls that are still
   running, so they can push spend past the limit. `limits.per_principal` bounds how many calls one
   principal can have running on each instance. A `tpm` limit counts the tokens of calls settled in
   the current minute: a call's tokens are added to the minute in which it is settled, and once they
   reach the limit, new calls are rejected with `rate_limited` until the next minute.
3. **Settlement.** The gateway verifies the provider's final usage from the complete response or
   stream: the terminal event, the model name, and usage fields that match standard pricing. The
   adapter reports the usage as a token count per [usage category](#providers-and-profiles), plus
   the reasoning count. The rates come from the price snapshot: the copy of the profile's rates
   that the gateway stored with the request when it admitted it. For each category, the gateway
   multiplies the tokens by the category's rate. It adds the results and rounds the sum once
   (fixed-point, 12 decimals). If a category has tokens but no rate, or the API did not declare the
   category, the cost is `unknown`, never 0. The result is recorded once. The request's usage record keeps the
   token count of each category (`input_tokens`, `output_tokens`, `cache_read_tokens`,
   `cache_write_tokens`, `cache_write_5m_tokens`, `cache_write_1h_tokens`), and the request detail
   on *Admin → Requests* shows them. For `tpm` limits and the ledger's per-call token totals, a
   call's input tokens are `input` plus every cache category, and its output tokens are `output`.
4. **No final usage.** If the final usage cannot be verified, the request is settled by why it
   ended (see decision [0005](docs/decisions/0005-gateway-prices-and-cut-off-requests.md)):
   - **An Anthropic stream cut off after its start event**: the client disconnected, the gateway
     shut down, the provider stopped sending, or the provider sent an error event. Input tokens are
     the values in `message_start`. Output tokens follow the cut-off rule on *Admin → Settings*,
     which a request keeps from the moment it is admitted:
     - `received` (the default): the text, thinking and tool input received before the cut-off,
       counted with Anthropic's free `count_tokens` endpoint. The content is kept in memory only, at
       most 8 bytes per `max_tokens` token and 2 MiB, and a single piece of output larger than
       6 KiB is not kept. If the content could not be kept, or the count fails, the request is
       settled as `max_tokens`. Thinking arrives only as a summary, so this can be less than the
       provider bills.
     - `max_tokens`: the request's `max_tokens`.

     Output never exceeds the request's `max_tokens`.
   - **No response**: the provider answered with an HTTP error (even if its body could not be
     read), a stream was cut off before its first event (for Anthropic, before `message_start`), or
     a request that is not streamed was cut off before its response. The request is settled at
     0 USD.
   - **Otherwise `unknown`**: the process stopped during the request, the reported model is not in
     `actual_models`, a usage field is unexpected or non-standard, a usage category has tokens but
     no rate, a price cannot be resolved, or an OpenAI or DeepSeek stream was cut off after it
     started. Unknown costs are never estimated.
     While any of a principal's requests are `unknown`, that principal's paid calls are rejected
     with `unsettled_usage_block`. An admin resolves each request on *Admin → Requests*
     (`PUT /api/admin/requests/{id}/settlement` with `decision: charged|no_charge`, `cost_usd` and
     `reason`).
5. **Safety pause.** If the settled cost exceeds the cost bound computed at admission, the gateway
   pauses inference for that principal (`inference_paused`). An admin must release the pause
   explicitly (`/api/admin/principals/{id}/inference-pause/release`). Policy edits do not clear it.

### Token check against the provider (optional)

The gateway can compare its own token counts with Anthropic's usage report. This works only when the
environment variable `ANTHROPIC_ADMIN_API_KEY` holds an Admin API key. Without it, the gateway does
not poll, *Admin → Token check* says that the check is off, and correction cannot be turned on.

- Every 5 minutes, one instance reads the usage report (`/v1/organizations/usage_report/messages`)
  in one-hour buckets, grouped by API key and model. It reads the last 48 hours each time, because
  the provider's data can arrive late, and it also reads any hours it missed in the last 30 days.
  Hours that ended less than 10 minutes ago are skipped.
- The report names keys by Anthropic's API key ID, not by the key itself. The gateway finds the ID of
  each provider key in the Admin API's list of API keys, which shows the first and last characters
  of each key. A provider key that is not found is listed on the screen and left out of the
  comparison.
- For each provider key, model and hour, the gateway compares the report with the tokens of its own
  requests in that hour. It compares each usage category separately: `input`, `cache_read`,
  `cache_write_5m`, `cache_write_1h` and `output`. Like the report, the gateway's own totals keep
  five-minute and one-hour cache writes apart ([decision 0012](docs/decisions/0012-usage-categories.md)).
  The gateway computes its own totals when it
  stores the report, so the screen reads stored totals. A difference sends no alert.
- *Admin → Token check* (`/dashboard/admin/token-check`) shows the tokens that the provider counted
  more and the tokens that the gateway counted more, priced at the current catalog prices. Each usage
  category is priced at its own rate; the gateway never uses another category's rate in its place.
  A model that has no rate for a category with tokens is listed as unpriced. The
  tokens that the provider counted more are split by reason. Each hour gets the first reason that
  applies: use of the key outside the gateway (no gateway request with that key in that hour),
  cut-off requests in that hour, requests with unknown usage in that hour, or unexplained.

**Correction.** The cut-off rule on *Admin → Settings* has a correction switch, off by default. When
it is on, the gateway also reads the report in one-minute buckets, and 10 to 15 minutes after a
cut-off request ends it replaces the request's output tokens with the provider's count.

- Anthropic's documentation does not say whether a one-minute bucket holds the requests that were
  sent in that minute or the requests that ended in it. This document calls it the bucket
  assignment: by send time or by end time.
- The gateway learns the bucket assignment first. It uses minutes in which every request on the
  provider key and model was settled with the usage that the provider sent in its response. Such a
  minute counts for an assignment when the bucket's tokens equal the sum of the requests that this
  assignment puts in the minute, and do not equal the sum for the other assignment. The gateway uses
  an assignment once at least 3 minutes count for it and the other assignment has no more than a
  tenth of that number. It corrects nothing before that. *Admin → Token check* shows how many
  minutes counted for each assignment.
- With the known assignment, the gateway corrects a request only when all of these hold:
  - every other request that the assignment puts in the same minute, with the same provider key
    and model, was settled with the usage that the provider sent in its response;
  - the request is more than 5 seconds away from a minute boundary;
  - the bucket's input, cache-read, five-minute cache-write and one-hour cache-write tokens, minus
    those of the other requests, each equal the request's own.

  The new output never exceeds the request's `max_tokens`.
- The cost change goes to the request's original budget period. The request detail shows the
  correction, and an audit record that the gateway writes itself (operation `usage_correction`)
  records it. The gateway tries each cut-off request from 10 minutes to 2 hours after it ended. The
  report usually arrives within minutes, so a request that cannot be corrected by then is left as
  it is.

`GET /api/admin/token-check?range=24h|7d|30d` (admin only) returns what the screen shows.


## Anthropic pass-through policy

Clients such as Claude Code add new request fields often. For that reason, Anthropic Messages and
count_tokens requests are validated strictly **only where cost is affected**. Everything else is
forwarded unparsed.

- **Validated:** `model`, `max_tokens` (≤ profile `max_output_tokens`), `stream`, message roles
  and content block types, thinking, client tool definitions, PNG image sources, `cache_control`
  (`ttl` `5m`, the default, or `1h`), `output_config.format`, the `type` of each
  `context_management` edit, and `tool_use.caller` (only `{"type": "direct"}`).
- **Cache lifetimes:** a five-minute and a one-hour cache write have different prices. A
  `messages` request that asks for a cache lifetime whose rate the profile does not have
  (`cache_write_5m` or `cache_write_1h`) is refused with `400 unsupported_option` and is not sent.
- **Rejected:** `service_tier`, `speed`, `inference_geo`, `mcp_servers` and `container`, both at the
  top level and inside messages. Also rejected: server tools, a non-`direct` tool_use `caller`,
  context edits other than `clear_*` (compaction, for example), unapproved betas, and any
  `anthropic-version` other than `2023-06-01`.
- **Passed through byte for byte:** all other options, such as `metadata`, per-message
  `output_config`, extra client-tool fields (`defer_loading`, `strict`) and `output_config.effort`.
  The overall body size, JSON depth and structure limits still apply.

Response usage follows the same rule. Values that match standard pricing settle normally
(`service_tier` `standard`, `speed` `standard`, `inference_geo` `global` or empty, zero server-tool use).
Any other value, or any unknown usage field, makes the request's cost `unknown`. Cache writes are
priced by lifetime: `cache_creation.ephemeral_5m_input_tokens` at `cache_write_5m` and
`cache_creation.ephemeral_1h_input_tokens` at `cache_write_1h`. If the two do not add up to
`cache_creation_input_tokens`, or they are missing while cache writes are not zero, the cost is
`unknown`.

### Using Claude Code

Claude Code works against the gateway. This was tested with Claude Code 2.1.322 and the real
Anthropic API, covering plain chat, tool round-trips with thinking, and streaming. Example
`~/.claude/settings.json`:

```json
{
  "env": { "ANTHROPIC_BASE_URL": "https://gateway.example.com/anthropic" },
  "apiKeyHelper": "/usr/local/bin/print-gateway-token"
}
```

`apiKeyHelper` must print the user's virtual key to stdout (an SSO access token is not accepted for
inference). Claude Code sends that value in both `Authorization` and `x-api-key`.

The profile must accept everything Claude Code sends for the model you use. With
`claude-opus-5-5`, Claude Code 2.1.322 sends `max_tokens: 128000` and these eight betas:

```
claude-code-20250219, interleaved-thinking-2025-05-14, context-management-2025-06-27,
per-turn-control-2026-07-01, mid-conversation-tool-changes-2026-07-01,
thinking-token-count-2026-05-13, advanced-tool-use-2025-11-20, effort-2025-11-24
```

With a model name it does not recognize, it sends a different set (7 betas, `max_tokens: 32000`).
`config.example.yaml` contains a `claude-opus-5-5` profile for the eight-beta set, with features
`[client_tools, thinking, cache]`.
Known limitations:

- Claude Code may request other models, for example for background tasks. Each model it uses needs
  its own profile.
- Reading JPEG images or PDFs in Claude Code fails, because only inline PNG is supported.
- When a new Claude Code release adds a beta, add it to `approved_beta`.

## Dashboard

The dashboard is at `/dashboard/`. Requests to `/` and `/dashboard` redirect there. The dashboard is served from the built bundle under a strict CSP.
It has no inline scripts and loads nothing from external CDNs.

| Screen | Path | Audience |
|---|---|---|
| My usage and budget balances | `/dashboard/` | users |
| My virtual keys (list, edit and revoke) | `/dashboard/keys` | users |
| Issue a virtual key | `/dashboard/keys/new` | users |
| Sign-in and setting a password from a link (`password` mode) | `/dashboard/signin`, `/dashboard/password` | everyone |
| Access list (who may sign in, who is an admin) | `/dashboard/admin/members` | admins |
| Principals, policies, service principals, safety-pause release | `/dashboard/admin/principals` | admins |
| Issue a virtual key for a service principal | `/dashboard/admin/principals/key?principal_id=<id>` | admins |
| All virtual keys: list them, edit them or revoke them | `/dashboard/admin/keys` | admins |
| Organization-wide usage | `/dashboard/admin/usage` | admins |
| Requests and manual settlement | `/dashboard/admin/requests` | admins |
| Provider keys (register, replace, retire) | `/dashboard/admin/credentials` | admins |
| Model & price catalog (sync, compare, approve, edit rates) | `/dashboard/admin/models` | admins |
| Token check against Anthropic's usage report | `/dashboard/admin/token-check` | admins |
| Settings (budget time zone, cut-off rule, correction) | `/dashboard/admin/settings` | admins |

The JSON API behind it is under `/api/` (`me`, `usage`, `csrf`, `keys`, and the `admin/*`
routes). It is same-origin only, with no CORS. The UI is in Korean by default; a language switch in the header changes it to English (the choice is remembered in that browser only).

## Operations

- **Health:** `GET /livez` returns `200 ok` once HTTP is up and makes no external calls.
  `GET /readyz` returns `200 ready` or `503 not_ready`, based on periodic checks of Postgres and, in `sso` mode,
  the JWKS.
- **Shutdown:** on `SIGTERM` the gateway stops reporting ready and rejects new inference and management writes.
  Requests in flight then get up to about 25 s to finish and settle, and the process exits within
  about 30 s. Give it a termination grace period of more than 30 s.
- **Telemetry:** when stderr is a pipe or socket, the gateway writes JSON lines of fixed-name
  counters and gauges to it (`{"telemetry": {...}}`). Metrics never include user strings, request
  bodies, tokens or secrets.
- **Migrations:** automatic at startup, as described in [Quick start](#1-prerequisites).
- **Changing access:** changes to the access list, policies and keys take effect on the next
  request. No restart is necessary.
- **Suspending an account:** an admin suspends a principal with the **Suspend account** button on
  its page under *Admin → Principals*. The API call is `PUT /api/admin/principals/{id}/policy` with
  the current grants and limits, `disabled: true` and a `reason`. No restart is necessary.
  - From the next request, the gateway rejects inference with that principal's virtual keys
    (`403 access_denied`).
  - A suspended person cannot use the dashboard or the `/api/*` routes.
  - The service principals that the person owns are also blocked (`403 owner_inactive`).
  - Nothing is deleted. The policy and the virtual keys stay as they are, and the virtual keys work
    again when an admin lifts the suspension.
  - An admin cannot suspend their own account. The gateway also refuses to suspend the last active
    admin.

### Model & price catalog

For providers configured with `catalog`, models and prices live in Postgres
(`gw_model_catalog_versions`). Nothing changes automatically:

1. An admin presses the sync button (**Sync with public price list**) on `/dashboard/admin/models`. The gateway downloads the public price list
   [models.dev](https://models.dev/api.json) and builds a proposal. No provider publishes an official
   pricing API, so this community list is the only price source; the gateway does not mix in other
   lists.
2. The proposal lists every catalog model whose prices or token limits changed, models that
   disappeared from models.dev, and new models whose name starts with one of the catalog's
   `model_prefixes`. For each one it shows the current value next to the proposed value.
   Some models cost more once the input exceeds a certain size (tiered pricing). The ledger stores
   a single price for each usage category, so for such models the proposed `max_input_tokens`
   stays below that size and requests above it are rejected.
   A sync proposes values only for the categories that models.dev gives: input, output, cache
   read and its one cache-write price. That cache-write price goes to `cache_write_5m` for Anthropic
   models. The other APIs declare no cache-write category, so a sync does not use it for them. A
   rate that models.dev does not give, such as `cache_write_1h`, keeps the value that an admin set.
3. The admin ticks the items to apply and approves. Changed and missing models start ticked, new
   models do not; a "select all" box in the table header ticks or clears every item at once. If
   another admin approved a different proposal after this proposal was built, the approval is
   rejected with `409 catalog_proposal_stale`; sync again to get a proposal based on the current
   catalog.
4. The approved list becomes a new catalog version. Each model becomes a profile with
   `pricing_version: catalog-v<N>` and is validated with the same rules as config profiles. Other
   instances load the new version at their next health check. Requests keep the price they were
   admitted with, so amounts already in the ledger never change.

**Editing rates.** An admin can set or clear the rate of any usage category that the model's API
declares ([decision 0012](docs/decisions/0012-usage-categories.md)). On `/dashboard/admin/models`, the **Edit rates** button of a model opens a form
with one field per category; an empty field means no rate. The form sends
`PUT /api/admin/model-catalog/rates`. Categories that the body does not name keep their rate. The
result must still be a valid profile: for example, an Anthropic model with the `cache` feature
cannot lose its `cache_write_5m` rate. Each change makes a new catalog version, like an approval.
A body that changes nothing returns `effect: noop` and makes no version. An Anthropic catalog model
has no `cache_write_1h` rate until an admin sets one, and until then its one-hour cache requests
are refused.

Every approval and every rate change is recorded in the command and audit logs with the before and
after values (operations `approve_model_catalog` and `edit_model_catalog_rates`). Policies can grant
catalog models like any other model.

| Method and path | Body | Result |
|---|---|---|
| `GET /api/admin/model-catalog` | – | Current version, entries and the catalog policy per provider, with the usage categories of its API (`usage_categories`) |
| `POST /api/admin/model-catalog/sync` | empty or `{}` | `201` with `proposal_id` and the proposal; `502 catalog_sources_unavailable` if models.dev could not be downloaded (the code keeps its earlier plural name) |
| `POST /api/admin/model-catalog/approve` | `{"proposal_id": "<uuid>", "expected_version": "<n>", "items": [{"provider": "...", "model": "..."}]}` | `200` with the new `result_version` |
| `PUT /api/admin/model-catalog/rates` | `{"provider": "...", "model": "...", "rates": {"<category>": "<USD per 1M tokens>" \| null}, "expected_version": "<n>"}` | `200` with the new `result_version`; `400 invalid_request` for a category that the API does not declare, a rate with more than 12 decimal places, or a result that is not a valid profile; `404` for a model that is not in the catalog; `409 version_conflict` if the catalog changed |

All three writes need `X-Gateway-Command-Id` and replay like other admin commands.

### Provider keys

Provider keys are the providers' own API keys (for example an Anthropic API key). They are stored in
`gw_provider_credentials`. A provider can have several keys, and each key has a name (label) so
admins can tell them apart, for example one key per team or per billing account. There is no default
key: every request goes out with the provider key its virtual key is bound to.

- **Names.** A label is 1–60 characters with no leading or trailing spaces. Two keys in use for the
  same provider cannot share a label (case-insensitive). A retired key's label can be reused.
- **Provider key access.** A user (or service principal) can use only the provider keys an admin
  authorized for them, per provider, on the user's detail page. A user with no authorized key for a
  provider cannot use that provider.
- **Authorizing one key for many principals.** An admin can authorize one provider key for many
  principals (users and service principals) in one step. On the provider keys page, each key in use
  has an **Allow users** button. The button opens a list of all principals. The admin can search the
  list, filter it by kind (human or service), and select principals. The **Select all** checkbox at
  the top of the list selects or deselects only the principals that the current search and filter
  show. When the admin confirms, the
  dashboard sends one command (`POST /api/admin/provider-credentials/{provider}/{id}`, where `{id}`
  is the provider key id).
  - The command only adds authorizations for this provider key. It does not remove any
    authorization. To remove one, use the principal's detail page.
  - The command authorizes all the listed principals or none of them. If one principal id does not
    exist, the gateway rejects the command with `404` and authorizes nobody.
  - A principal that is already authorized for the key stays authorized. If all the listed
    principals are already authorized, the result has `"effect": "noop"`.
  - One command accepts 1 to 500 principal ids. A repeated id is rejected with `400`.
  - The command does not change which provider key a virtual key is bound to. A virtual key can be
    blocked because its owner is no longer authorized for the provider key it is bound to. Such a
    virtual key works again when its owner is authorized for that provider key again.
- **Binding virtual keys.** A virtual key is issued on its own page, separate from the key list. To
  issue a virtual key, the caller must choose one authorized provider key for every provider in the
  key's grants (`"provider_credentials": {"anthropic": "<credential id>"}` in `POST /api/keys`).
  - If a provider in the grants has no chosen provider key, the gateway rejects the issue request
    with `400 provider_credential_required`.
  - If the grants are empty, the gateway rejects the issue request with the same error. Such a
    virtual key would have no provider key and could not be used for inference.
  - If the owner is not authorized to use a chosen provider key, the gateway rejects the issue
    request with `403 provider_credential_not_authorized`.

  The dashboard issue page asks for the provider keys first. Then it lists only the grants whose
  provider has a chosen provider key. After issue, the virtual key keeps using those provider keys.
  The owner or an admin can choose other provider keys when they edit the virtual key (see "Editing
  a virtual key" above). A new provider key must be one that the owner is authorized to use.
- **Removing access.** If you remove a key from a user's access while some of the user's virtual
  keys are bound to it, the change fails with `409 credential_authorization_in_use` and the number
  of those keys. Send it again with `bound_keys` set to one of these values:
  - `{"move_to": "<credential id>"}`: the virtual keys move to that key, which must stay authorized.
  - `"block"`: the bindings stay. Requests with those virtual keys to that provider are rejected
    with `403` and `x-gateway-error-code: provider_credential_not_authorized` before anything is
    sent. The key list shows `"authorized": false` on the binding, and the dashboard marks it
    **Access removed**. Authorizing the key again, or moving the virtual key to an authorized key,
    makes it work again.

  Requests already in progress finish with the key they started with.
- **Unbound virtual keys.** A virtual key with no provider key for a provider (for example after the
  last key of that provider was retired) gets `403 provider_credential_unbound` for that provider
  until an admin binds it to a provider key.
- **Encrypted at rest.** AES-256-GCM with a key derived from `CREDENTIAL_ENCRYPTION_KEY`. The provider
  name and row id are bound to the ciphertext, so a ciphertext copied to another row does not
  decrypt. The key itself is never returned by any API, written to the command or audit log, or
  printed. The dashboard shows only the last 4 characters.
- **Verified before saving.** Adding a key or replacing it always calls the provider's model-list
  endpoint first (`GET /v1/models` for Anthropic, OpenAI and TypeSafe, `GET /models` for DeepSeek;
  no charge).
  A `401`/`403` is rejected with `422 provider_credential_rejected`; any other failure with
  `502 provider_credential_unverified`. Nothing is stored in either case. The key must also match
  the provider's format (`sk-ant-…` for Anthropic, `sk-…` for OpenAI and DeepSeek; TypeSafe has no
  published format, so only the length and characters are checked). The same key
  cannot be registered twice (`409 credential_duplicate`).
- **Replacing a key.** Use this when a key leaks or expires. The new key gets a new row; the old row
  is retired and its ciphertext erased. The label, the users' access and the bindings of the virtual
  keys that are still in use move to the new row, so users do not need new virtual keys. Requests already in
  progress keep the key they started with. Other instances pick up the change at their next health
  check (every 5 s).
- **Retiring.** Retiring a key never makes a blocked virtual key usable again. Virtual keys that were
  blocked by removing access stay bound to the retired key and stay blocked until an admin binds them
  to another key. The other virtual keys bound to the key (not revoked or expired, and whose owners
  may use the key) are handled as follows:
  - If the provider has other keys, the request must name a replacement (`move_to`, another key of
    the same provider); without it, it fails with `409 credential_in_use`. Those virtual keys move to
    the replacement, and every user who could use the retired key can use the replacement.
  - If it is the provider's last key, no replacement is needed. Those virtual keys lose their binding
    and get `403 provider_credential_unbound` for that provider. Users' access to the retired key is
    removed.
- **Compare-and-swap.** Each key has an `id` and a `version`, both returned by `GET
  /api/admin/provider-credentials`. The version goes up whenever the key is renamed or retired.
  Rename, replace and retire requests must send the version you last read (`expected_version`); if
  another admin changed the key in between, the request fails with `409 version_conflict`. Moving a
  virtual key must send the provider key it is bound to now (`expected_credential_id`, from the key
  list) and fails with `409 credential_binding_conflict` if that changed. Changing a user's access
  must send the access you last read (`expected`) and fails with
  `409 credential_authorization_conflict` if it changed.
- **Traceability.** Every request records the provider key it used
  (`gw_requests.provider_credential_id`); the admin request list and detail show its label. You can
  find what a leaked key was used for.
- **Monitoring.** The `provider_credentials_unavailable` metric is the sum of two counts: configured
  providers with no provider key in use, and keys in use that cannot be decrypted with the current
  `CREDENTIAL_ENCRYPTION_KEY`. Zero means every key works. To recover a key that cannot be
  decrypted, replace it with the same provider API key (**Replace key**): its label, bindings and
  access stay. Adding it again as a new key does not help, because virtual keys stay bound to the
  old one. `/readyz` is not affected, so an admin can
  still reach the dashboard to register a key.

API (admin only; cookie writes need the usual `Origin` and `X-CSRF-Token`):

| Method and path | Body / query | Result |
|---|---|---|
| `GET /api/admin/provider-credentials` | – | For each configured provider: `status`, the keys in use and the last 10 retired keys. `status` is `missing` if no key is in use, `undecryptable` if a key cannot be decrypted, else `active`. Each key in use has `id`, `version`, its label, the number of bound virtual keys and `authorized_principal_ids` (the principals authorized for it) |
| `POST /api/admin/provider-credentials/{provider}` | `{"label": "...", "secret": "..."}` | `201`, key added |
| `PUT /api/admin/provider-credentials/{provider}/{id}` | `{"secret": "...", "expected_version": "<n>"}` | `201` key replaced (new id), `200` no-op if it is the same key |
| `PATCH /api/admin/provider-credentials/{provider}/{id}` | `{"expected_version": "<n>", "label": "..."}` | `200`, renamed |
| `DELETE /api/admin/provider-credentials/{provider}/{id}` | `?expected_version=<n>[&move_to=<id>]` | `200`, key retired |
| `POST /api/admin/provider-credentials/{provider}/{id}` | `{"principal_ids": ["<id>", ...]}` | `200`, the key is authorized for all the listed principals. `credential.granted_principal_ids` in the result lists the principals that were newly authorized |
| `PUT /api/admin/keys/{key id}/provider-credential` | `{"provider": "...", "credential_id": "<id>", "expected_credential_id": "<id>" \| null}` | `200`, virtual key moved to another provider key the owner may use |
| `PATCH /api/keys/{key id}` | `{"expected_version": "<n>", "name": "...", "grants": [...], "provider_credentials": {"<provider>": "<credential id>"}}` | `200`. The name, the grants and the provider keys of the virtual key are replaced. The owner and admins can call it |
| `PUT /api/admin/principals/{id}/provider-credentials` | `{"provider": "...", "credential_ids": ["<id>", ...], "expected": {"credential_ids": [...]}, "bound_keys"?: "block" \| {"move_to": "<id>"}}` | `200`, the user's access for that provider is replaced. An empty list removes access to that provider. `expected` is the access you last read (from `GET /api/admin/principals/{id}`) |

`GET /api/me` returns the caller's own authorized provider keys (`provider_credentials`), the
budget time zone (`budget_timezone`) and the highest limits a new key may have
(`key_limit_maximum`), so users can choose a key and its limits when issuing a virtual key.

Writes require an `X-Gateway-Command-Id` header. As with other admin commands, resending the same
command id returns the recorded result without running the command or calling the provider again.

### Maintenance mode

`m-gated --maintenance --config gateway.yaml` validates the configuration, connects to the
database, runs migrations and executes **one** command read from stdin. It does not open a listener.
stdin and stdout must be pipes or sockets, not regular files or TTYs. The input is a JSON envelope
that carries a current admin's SSO JWT (in `password` mode, a session token of a current admin, the
value of the `mgated_session` cookie). API keys are not accepted. Example that deletes settled
ledger detail older than `retention.ledger`:

```sh
printf '%s' '{"access_token":"<admin SSO JWT>","command_id":"<new UUID>",
  "command":{"operation":"retention_delete","after":null,"limit":50,
  "deletion_approved":true,"approval_ref":"<your change ticket>","reason":"monthly cleanup"}}' \
  | m-gated --maintenance --config gateway.yaml | cat
```

The output is `{"ack": {...}, "result": {"deleted_requests": N, "next_after": <uuid|null>, "request_ids": [...]}}`.
To continue, run the command again with `after` set to `next_after` and a **new** `command_id`.
Requests that are unknown or still in progress are never deleted. If the command fails, the gateway
prints `maintenance_failed` and exits with status 1.

The same mode can add, replace or retire a provider key without the dashboard. The key travels only
in the stdin envelope and is verified with the provider first:

```sh
printf '%s' '{"access_token":"<admin SSO JWT>","command_id":"<new UUID>",
  "command":{"operation":"add_provider_credential","provider":"anthropic",
  "label":"team-a","secret":"sk-ant-..."}}' \
  | m-gated --maintenance --config gateway.yaml | cat
```

Other operations:
`{"operation":"replace_provider_credential","provider":"anthropic","credential_id":"<id>","expected_version":1,"secret":"sk-ant-..."}`
and
`{"operation":"retire_provider_credential","provider":"anthropic","credential_id":"<id>","expected_version":1,"move_to":"<id>"}`
(`move_to` is optional, with the same rules as the API). The output is `{"status": 201|200, "ack": {...}}`.

## Development and tests

```sh
cargo test --locked --workspace                      # unit + integration suites, no external services
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
(cd web && npm ci && npm run typecheck && npm test)  # dashboard unit tests (Vitest)
```

The DeepSeek and OpenAI adapters can also be built as WebAssembly (WASM) components. This is a
trial ([decision 0009](docs/decisions/0009-deepseek-adapter-as-wasm-trial.md),
[decision 0013](docs/decisions/0013-openai-adapter-as-wasm.md)). The adapter logic is in ordinary
Rust crates, `adapters/deepseek` and `adapters/openai`, which the gateway also links directly as
its built-in adapters. The crates `adapters/deepseek/component` and `adapters/openai/component`
only convert between the interface types and the types of those crates. Code that the gateway and
the adapters share, such as the JSON request checker, the stream decoders and the PNG image check,
is in `crates/adapter-kit`.

The interface is the WIT package `m-gated:adapter@0.3.0` in `crates/adapter-kit/wit/adapter.wit`.
A component exports its one interface, `provider`, so the export name is
`m-gated:adapter/provider@0.3.0`. The gateway accepts only version 0.3.0.

- At start, the gateway calls `describe()`. It returns a `description` record with the provider's
  fixed facts: its name, the upstream address, the key rules, the provider's name in the models.dev
  price list (`models-dev-name`, or none), and a list of `api-info` records.
- Each `api-info` record describes one API: its routes, upstream path, error format, usage
  categories, whether it streams, and whether it can use the model catalog (`catalog`).
- The `profile` record is the part of a model profile that the component sees. For OpenAI it also
  has `chat_output_limit_field`, `embedding_dimensions` and `embedding_encodings`.
- The function `read` reads a non-streamed response. It also gets the client's request body,
  because the Embeddings adapter checks the response against the number of inputs and the vector
  size that the request asked for.
- The `usage` record has three parts: the actual model, a list of category and token count pairs,
  and the reasoning count ([decision 0012](docs/decisions/0012-usage-categories.md)). To
build the components and run their tests:

```sh
scripts/build-adapter.sh   # writes target/adapters/{deepseek,openai}.wasm and their .sha256 files
cargo test --locked --test wasm_adapter -- --ignored
```

The tests compare each component with its built-in adapter, and check the memory and CPU limits
and the refusal of wrong files, including components that export interface version 0.1.0, 0.2.0 or 0.3.1. A unit
test in `src/providers/wasm.rs` checks that a description with a wrong `catalog` or
`models-dev-name` is refused.

The script adds the `wasm32-unknown-unknown` target, installs `wasm-tools` 1.259.0 in
`target/tools`, and refuses a component that imports anything or that does not export
`m-gated:adapter/provider@0.3.0`. It replaces the absolute paths of the source and the Cargo home
with fixed names, so the files do not contain your paths. A `docker build` of the release
Dockerfile builds the components in its `adapter-build` stage. The components' code compiles only
for wasm32, so check it with:

```sh
cargo clippy --locked --target wasm32-unknown-unknown -p m-gated-adapter-deepseek-component -- -D warnings
cargo clippy --locked --target wasm32-unknown-unknown -p m-gated-adapter-openai-component -- -D warnings
```

The Postgres-backed suites are marked `#[ignore]`. They need a dedicated local database at exactly
`postgres://m_gated_test@127.0.0.1:64603/m_gated_test`: no password (trust authentication), no TLS
and no `PG*` environment variables. Each test creates and removes its own schema. Example:

```sh
export TEST_DATABASE_URL=postgres://m_gated_test@127.0.0.1:64603/m_gated_test
cargo test --locked --test postgres_contract -- --ignored
cargo test --locked --test auth_postgres -- --ignored
```

To run a provider's Postgres suites against its WASM component instead of the built-in adapter,
build the components and give the component's path in an environment variable:
`TEST_DEEPSEEK_ADAPTER` for `deepseek_postgres`, and `TEST_OPENAI_ADAPTER` for
`responses_postgres` and `chat_embeddings_postgres`. Each test then checks that the component is
installed. To measure the DeepSeek component's run time and size against the built-in adapter, run the
measurement suite in release mode. It prints times and fuel, and checks nothing.

```sh
scripts/build-adapter.sh
TEST_DEEPSEEK_ADAPTER=$PWD/target/adapters/deepseek.wasm \
  cargo test --locked --test deepseek_postgres -- --ignored --test-threads=1
TEST_OPENAI_ADAPTER=$PWD/target/adapters/openai.wasm \
  cargo test --locked --test responses_postgres -- --ignored --test-threads=1
TEST_OPENAI_ADAPTER=$PWD/target/adapters/openai.wasm \
  cargo test --locked --test chat_embeddings_postgres -- --ignored --test-threads=1
cargo test --release --locked --test wasm_adapter_cost -- --ignored --nocapture --test-threads=1
```

The management API has a record test, `tests/management_routes_postgres.rs`. It fails when an
answer of the management API changes. The test sends requests to each management path, and to a
few paths that look like them but do not exist. It uses each method, requests without a credential
and with several kinds of credential, and several bad inputs. For each answer, it writes one line
with these values:

- the status;
- the error code in the JSON body;
- the `Allow`, `Location`, `Content-Type`, `x-gateway-error-code` and `Retry-After` headers.

Then the test compares the lines with `tests/golden/management_routes.txt`. When you intend to
change an answer, write the file again with `UPDATE_GOLDEN=1`. Then read the changes in the file
with `git diff` before you commit the file.

```sh
cargo test --locked --test management_routes_postgres -- --ignored
UPDATE_GOLDEN=1 cargo test --locked --test management_routes_postgres -- --ignored
```

The runners in `scripts/` handle the full regression: SDK compatibility through `tests/compat`,
the real binary through `tests/process`, and browser end-to-end tests with Playwright. They also
need Docker, `TEST_FNM_DIR` (a directory of the fnm Node version manager with Node 22.22.2 installed) and, for browser tests,
`TEST_PLAYWRIGHT_BROWSERS_PATH`. All of these use synthetic SSO, providers and credentials. They
never contact real services.

### Dashboard preview without a server

You can look at the dashboard in a browser without running m-gated and without a database:

```sh
cd web && npm ci && npm run dev
```

Open `http://localhost:5173/dashboard/`. This command starts the Vite development server. The
development server answers `/api/*` and `/auth/*` with fake data from `web/mock/`. When you edit the
code under `web/src`, the page updates immediately.

- You are signed in as an admin, and every screen has data: users, provider keys, virtual keys,
  requests, models and usage.
- Writes change the fake data in memory, so a flow such as "issue a key, edit it, revoke it" runs to
  the end. Restart the development server to get the initial fake data back.
- To see an error screen, use these inputs:
  - When you add or replace a provider key, type an API key that contains `reject`. The fake API
    answers as if the provider rejected the key.
  - When you add or replace a provider key, type an API key that contains `unverified`. The fake API
    answers as if the check with the provider failed.
  - When you issue a virtual key, type a name that starts with `quota`. The fake API answers that
    the user has too many keys.
- The preview does not show everything. You are always an admin, so you cannot see the screens of a
  normal user or the permission errors (401, 403). The fake API returns only a few of the server's
  errors, such as a version conflict (409) and a provider key label that is already in use. The
  browser tests (`web/e2e`, run by `cargo test --test dashboard_e2e` and `--test admin_e2e`) run
  against the real server.
- The fake API is part of the development server only. `npm run build` and the dashboard unit tests
  (`npm test`) do not include it.

## Design decisions

[`docs/decisions/`](docs/decisions/README.md) records why a design was chosen and which
alternatives were rejected, one file for each decision.

## Releases

Pushing a `vX.Y.Z` tag runs [`.github/workflows/release-image.yml`](.github/workflows/release-image.yml):

1. `verify`: dashboard tests and build, `cargo fmt`, `clippy` and `cargo test` (the `#[ignore]`d
   suites above are not run in CI), and the WASM adapters' clippy, build and tests. The DeepSeek
   and OpenAI component files go to the `build` job.
2. `build`: the image is built natively on amd64 and arm64 runners, with the component files from
   `verify` (`--build-context adapter=...`) and the labels `m-gated.adapters.deepseek.sha256` and
   `m-gated.adapters.openai.sha256`, pushed by digest, and checked
   with `scripts/test-package.sh` (non-root, no Node/npm/cargo in the runtime layer, read-only root,
   each component file matches its SHA-256 and cannot be written, refuses to start without a valid
   config).
3. `publish tags`: both digests are combined into one multi-arch manifest tagged `X.Y.Z`, `X.Y`,
   `X` (from 1.0 on), `latest` (non-prerelease only) and `sha-<short>`.

A manual run from the Actions tab publishes `<branch>` and `sha-<short>` tags instead.

## License

[MIT](LICENSE)
