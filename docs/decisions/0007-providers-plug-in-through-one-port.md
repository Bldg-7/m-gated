# 0007. Providers plug in through one port

- Date: 2026-11-25
- Status: Implemented

## Context

Provider-specific code was spread through the gateway. The request handler, the send code, the
config check, the ledger, the provider key check and the catalog sync each chose their behavior
with a `match` on the provider name (`ProviderId`) or the API name (`ApiKind`). Anthropic and the
OpenAI-compatible providers also had two separate loops that relay a streamed response, with small
differences. To add a provider, a developer had to find and change all of these places.

A later step may let an operator add a provider without changing the gateway's code. That needs
one narrow boundary between the gateway and provider code first.

## Decision

The gateway reaches all provider-specific behavior through one port. The gateway behaves the same
as before, except for two rare responses (see Consequences).

1. `src/providers/port.rs` defines the port with three traits. `PROVIDERS` lists every adapter.
   - `Provider` is one provider. It gives:
     - the upstream origin and the auth header,
     - the key prefix check (for example, an Anthropic key starts with `sk-ant-`), which stops an
       admin from registering one provider's key for another,
     - the free GET path that the gateway calls to test a new provider key,
     - the provider's APIs.
   - `Api` is one API of a provider, for example Anthropic Messages. It gives:
     - the routes that the gateway serves for the API, and the upstream path,
     - the request check and the client headers to forward,
     - the reading of a non-streamed response body,
     - the token rule (defined in rule 6).
   - `StreamReader` reads one streamed response, one event at a time. For each event it returns a
     `Step`, which says whether the event ends the stream, whether it is an error event, and which
     output text it carries.
2. An adapter only reads and reports. It does no network or file I/O. One case needs an extra
   request: under the `received` settlement rule of decision 0005, the gateway counts the output
   text of a cut-off Anthropic stream with the free `count_tokens` API. The Anthropic adapter
   builds the body of that request. The gateway sends it through `SideRequest`, with the same
   provider key as the user's request, a deadline of at most two seconds and a 4 KiB limit on
   the answer.
3. The gateway keeps every rule that protects money and users:
   - It sends the request once, without redirects, to the configured origin.
   - It checks the status, content encoding, content type and the size of the response body.
   - It relays the bytes to the client in the order they arrive. While an error event may still
     be too large, it holds back at most one chunk.
   - It settles a request without final usage at 0 USD, by the settlement rule, or as `unknown`.
     If an adapter reports no usage, the request is `unknown`.
   - It blocks users and enforces deadlines.
4. One stream relay loop serves all providers. The differences between the two old loops are now
   answers from the adapter:
   - which event ends the stream (`Step.terminal`),
   - which event is an error event (`Step.error`),
   - which output text to keep for the `received` rule (`Step.content`),
   - the input usage from the start event (`StreamReader::start`),
   - whether to stop reading after an error event (`StreamReader::stops_at_error`). Only Anthropic
     does, because it closes the connection after an error.
5. The settlement rule of decision 0005 applies to a stream that is cut off after it starts only if
   the API's start event reports the input tokens (`Api::settles_cutoff`). When the ledger admits
   such a request, it records the settlement rule that the admin set for the request's provider.
6. Price arithmetic stays in the ledger. Each request's price snapshot (the prices copied into the
   request when it is admitted) names a token rule. The token rule says which token counts the
   input and output prices cover, for example whether input tokens include cache reads. An
   adapter picks one of the token rules that the ledger knows (`Api::token_rule`). A new way to
   count tokens needs a ledger change.
7. A profile is one model entry in the config file (see the README). Profile rules that apply to
   one provider are in `Provider::check_profile`. Rules for fields that only one API has
   (`chat_output_limit_field`, `embedding_dimensions`, `embedding_encodings`, `count_billing`) stay
   in the config check and use `ApiKind`.
8. To add a provider in code, a developer adds a `ProviderId` name, implements `Provider` and its
   `Api`s, and adds the provider to `PROVIDERS`. A new kind of API also needs an `ApiKind` name.
9. The token check and correction uses the Anthropic Admin API. It is an Anthropic feature and stays
   outside the port.

## Rejected alternatives

**Put the boundary at the relay level, so that each adapter sends its own request and runs its own
relay loop.** This was a smaller change. It was rejected because each adapter would have to repeat
the checks in rule 3. A mistake in one adapter would then break those checks for its provider.
An adapter that does I/O also cannot run in a sandbox later.

**Stop using the fixed `ProviderId` and `ApiKind` lists, and read provider and API names from
configuration when the gateway starts.** This was deferred to the step that lets operators add
providers. Database rows, the config file and the grants in policies store these names, and some
database columns allow only the three current provider names. The change is larger than the port.

## Consequences

- The gateway behaves the same except for two rare responses from OpenAI or DeepSeek:
  - A streamed request that gets a 2xx status other than 200 is now a protocol error, as it already
    was for Anthropic. A protocol error leaves the request `unknown`. Before, the gateway read the
    body as a non-streamed JSON response.
  - The gateway reads at most 64 KiB of an error response body, and at most the profile's
    `max_response_bytes` of a success body. A non-streamed 2xx response other than 200 now gets
    the success limit. Before, it got 64 KiB.
- A provider adapter can be written and tested without reading the gateway, the ledger or the
  config check.
- Changed code: `src/providers/` (new `port.rs` and `relay.rs`; one adapter per provider),
  `src/gateway.rs`, `src/config.rs`, `src/ledger/finance.rs`, `src/credentials.rs`,
  `src/catalog.rs`, `src/http.rs`. The README does not change, because the gateway behaves the
  same.
