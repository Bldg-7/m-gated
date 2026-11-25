# 0008. Add TypeSafe, and decompress zstd request bodies in the gateway

- Date: 2026-11-25
- Status: Implemented

## Context

TypeSafe's models (Jev) answer typed questions about content that the client sends: the
probability that a statement is true, one choice from a list, or a score on a scale. The API has
one inference endpoint, `POST /v1/systemone`, with a bearer key. It does not stream. TypeSafe bills
input tokens only. Output tokens are free.

Two facts about the TypeSafe SDKs limit how the gateway can serve them:

- The SDKs compress every request body of 1 KiB or more with zstd by default
  (`Content-Encoding: zstd`). The gateway must read a request to check it and to admit it into the
  ledger. Before this decision it rejected every request with a `Content-Encoding` other than
  `identity`.
- The SDKs send images as `multipart/form-data`. The gateway accepts JSON request bodies only.

## Decision

TypeSafe is a fourth provider. The gateway decompresses a zstd request body from a TypeSafe
client, checks the decompressed body, and sends the decompressed body to TypeSafe. Images are not
supported.

1. TypeSafe is added through the provider port (decision 0007). The provider name is `typesafe`,
   the API name is `systemone`, and the gateway route is `/typesafe/v1/systemone`.
2. The request check accepts `model`, `state` and `questions`. Each question is `noul`, `choice`
   or `score`, with the fields and limits of the TypeSafe API reference. The following get
   `unsupported_option`:
   - `assets` and `box` questions, which need images,
   - the `TypeSafe-Danger-Zone-Images` header, which turns on images,
   - the `TypeSafe-Danger-Zone-Long-Context` header, which turns on a larger context window.
3. Decompression applies only to an API that accepts it (`Api::accepts_zstd`; only TypeSafe does):
   - `Content-Encoding` must name exactly one `zstd` layer. Any other encoding gets
     `415 unsupported_media_type`.
   - The body must be exactly one zstd frame (one compressed unit), and the frame's window (the
     memory that the decoder needs) may be at most 8 MiB. A larger window is rejected before any
     memory is reserved. Otherwise the request gets `400 invalid_request`.
   - Two size limits apply, in this order. First, the decompressed body must fit the server-wide
     request size limit (`limits.inference_body_bytes`), or the request gets
     `413 body_too_large`. Then the model profile's own limit (`max_body_bytes`) applies to the
     decompressed body, as it does to every request.
   - The gateway sends the decompressed bytes upstream without a `Content-Encoding`.
4. The gateway settles a TypeSafe request from `usage.input_tokens` and `usage.output_tokens`. If
   the usage has any other field, the request is `unknown`. The token rule is
   `typesafe_input_output_v1`.
5. A TypeSafe profile has exactly two rates, `input` and `output`, and `output` must be 0. When the
   gateway admits a request, it computes the highest cost that the request can have (the cost
   bound). If the settled cost is higher, the gateway pauses all inference for that user until an
   admin releases the pause. A TypeSafe request has no output limit, so its cost bound covers only
   input tokens. A non-zero output rate could make the settled cost exceed the bound.
6. If TypeSafe answers with an HTTP error, the gateway settles the request at 0 USD, as for other
   providers. If the error body is a JSON object, the client gets it unchanged. Any other error
   body is not relayed, and the client gets the gateway's `upstream_protocol_error`. Errors that
   the gateway itself creates use TypeSafe's format, `{"detail": "..."}`.
7. The gateway passes `x-typesafe-request-id` and `retry-after-ms` to the client and records
   `x-typesafe-request-id` as the provider request id.
8. TypeSafe does not publish a key format. The gateway checks only the key's length and
   characters. The key check before registration (`GET /v1/models`) rejects a key that TypeSafe
   does not accept.

## Rejected alternatives

**Ask users to turn off compression in the SDK.** Every user would have to change their client.
Until they did, most requests would fail with 415.

**Check the decompressed body, but forward the compressed body.** The upload would be smaller. But
the gateway would then rely on TypeSafe decompressing the same bytes that the gateway checked.

**Also accept gzip.** The SDKs do not send it.

**Accept images through `multipart/form-data`.** This needs a new way to read and check requests.
It is deferred until a user needs images.

## Consequences

- The gateway depends on `ruzstd`, a zstd decoder written in Rust.
- Migration 0018 adds `typesafe` and `systemone` to the database checks. The
  `gw_settlement_rules` table, which holds the admin's cut-off settlement rule for each provider,
  does not change, because TypeSafe has no streams.
- Decision 0007 meant to remove every fixed list of provider or API names outside the port, but
  six remained. Each one now asks the port, or uses the provider names in `ProviderId::ALL`:
  - the check that a path is an inference route, which picks the response time limit
    (`src/server.rs`),
  - the error format used when the server is not ready to take a request (`src/http.rs`),
  - the check that a grant names an API of its provider, when a policy is saved and when a request
    is admitted,
  - the API names allowed in the execution record that the ledger stores with each request,
  - the provider filter of the admin request list and of the usage API.
- The README describes the TypeSafe route, request rules, compression and rates.
