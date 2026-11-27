# 0012. Report usage in the gateway's usage categories, with each API declaring the ones it uses

- Date: 2026-11-27
- Status: Implemented

## Context

This decision lets a provider adapter report usage in more kinds of tokens than today, so that a
cache policy with its own price, such as Anthropic's one-hour cache, can be priced instead of
refused. The gateway keeps the list of kinds (usage categories), and each API declares which of
them it uses.

Terms used below:

- A **profile** is one model's entry, with its limits and its rates: either in the config file or in
  the model catalog that admins approve. Each approval makes a new **catalog version**.
- The **price snapshot** is the copy of a profile's rates and pricing rules that the gateway stores
  with a request when it admits it (`pricing_snapshot`). The request is priced with it later.
- The **normalized usage** is the usage that the ledger stores for a request (`normalized_usage` in
  `gw_usage_events`).

The providers are Anthropic, OpenAI, DeepSeek and TypeSafe.

Today the adapters and the ledger meet in a fixed usage shape (`FinalUsage` in Rust, the `usage`
record in the WASM interface): input, output, cache read, cache write and reasoning tokens. Most
profiles and the catalog use four rates (input, output, cache read, cache write). But a price
snapshot already accepts rates in six categories: `input`, `output`, `cache_read`, `cache_write`,
`cache_write_5m` and `cache_write_1h`. Because the usage has only one cache write count, a snapshot
rule, `cache_tier_rules = anthropic_5m_v1`, prices that count at the `cache_write_5m` rate for
Anthropic.

Anthropic charges a different price for a cache write that lives one hour than for one that lives
five minutes, and reports the two counts separately (`cache_creation.ephemeral_5m_input_tokens` and
`ephemeral_1h_input_tokens`). Because the usage shape has one cache write count, the gateway refuses
a request that asks for the one-hour cache. If a response still reports one-hour cache writes, the
cost becomes `unknown`.

The models.dev price list, which the catalog sync reads, gives one cache write price per model.

## Decision

1. **One list of categories.** The gateway defines the usage categories, and they are the same as
   the price categories: `input`, `output`, `cache_read`, `cache_write`, `cache_write_5m` and
   `cache_write_1h`. `cache_write` is a cache write whose price does not depend on how long the cache
lives. Reasoning tokens stay a
   separate count. They are part of `output` and are not priced again. A new category needs a
   change to the gateway.
2. **Each API declares its categories.** The provider port gives, for each API, the categories that
   its usage can have:

   | API | Categories |
   |---|---|
   | Anthropic `messages` | `input`, `output`, `cache_read`, `cache_write_5m`, `cache_write_1h` |
   | Anthropic `count_tokens`, OpenAI `embeddings` | `input` |
   | OpenAI `responses`, OpenAI `chat_completions`, DeepSeek `chat_completions` | `input`, `output`, `cache_read` |
   | TypeSafe `systemone` | `input`, `output` |

3. **Usage is a list.** An adapter reports usage as the actual model, a list of category and token
   count pairs, and the reasoning count. A category appears at most once. A category that the API
   did not declare makes the response malformed, so the cost becomes `unknown`. A declared category
   that is missing counts as 0.
4. **Pricing.** The cost is the sum, over the categories, of tokens times the rate in the request's
   price snapshot, rounded once (`sum_half_up_12_v1`, as now). If a category has tokens but the
   snapshot has no rate for it, the cost is `unknown`, never 0. The `anthropic_5m_v1` rule is
   removed, because Anthropic usage now reports the two cache lifetimes separately. An Anthropic
   snapshot has a `cache_write_5m` rate and, if an admin set it, a `cache_write_1h` rate. It cannot
   have a `cache_write` rate.
5. **Refuse what cannot be priced.** Before a request is sent, the adapter checks the categories
   that the request can cause. An Anthropic request may ask for `cache_control` with a `ttl` of
   `5m` (the default) or `1h`. If the profile has no rate for that lifetime, the request is refused
   with `400 unsupported_option`.
6. **Anthropic response usage.** `cache_creation.ephemeral_5m_input_tokens` goes to
   `cache_write_5m`, and `ephemeral_1h_input_tokens` to `cache_write_1h`. If their sum is not equal
   to `cache_creation_input_tokens`, or the split is missing while cache writes are not zero, the
   cost is `unknown`.
7. **Rates that the admin sets.** In the catalog, an admin can set or clear the rate of any declared
   category of a model: `PUT /api/admin/model-catalog/rates` with the provider, the model, the
   rates and the expected catalog version. Each change makes a new catalog version and is written to
   the command and audit logs, like an approval. The dashboard's models screen gets an "edit rates"
   form. models.dev gives one cache write price per model. A sync puts it in `cache_write_5m` for
   Anthropic models. The other APIs declare no cache write category (rule 2), so a sync does not use
   it for them. A sync proposes changes only to the categories that models.dev gives. A rate that models.dev does not give, such as `cache_write_1h`, keeps the
   value that an admin set.
   Config profiles set the same categories in `rates`.
8. **WASM interface `m-gated:adapter@0.2.0`.** `api-info` gets the list of usage categories, and the
   `usage` record becomes the actual model, a list of category and token count pairs, and the
   reasoning count. The host accepts only version 0.2.0. The DeepSeek component is rebuilt, so its
   SHA-256 in release images changes.
9. **Ledger and reports.** The normalized usage in the ledger records the token count of each
   category. The request detail on the dashboard shows them. The hourly usage that the token check
   stores (`gw_gateway_usage`) splits cache writes into `cache_write_5m` and `cache_write_1h`, the
   same split as Anthropic's usage report, and the token check compares them separately. The token
   totals that the ledger keeps for `tpm` stay as now: input tokens are `input` plus every cache
   category, and output tokens are `output`.

## Rejected alternatives

**Adapters name their own categories.** A new cache policy would then need no gateway change. But
the ledger, the hourly report, the token check and the dashboard would all have to handle names
that they do not know. Rejected to keep those parts simple and checkable.

**Keep refusing the one-hour cache and only widen the port.** Rejected because pricing the
one-hour cache is the main reason for this change.

**Fill missing rates from multiples of the base input rate.** For example, Anthropic documents a
five-minute write at 1.25 times and a one-hour write at 2 times the input price. Rejected because the
catalog is the gateway's own price list that an admin approves, and a derived rate that nobody
checked would go into the ledger.

## Consequences

- The WASM interface changes in a breaking way. A component built for version 0.1.0 is refused at
  start.
- An Anthropic catalog model has no one-hour cache rate until an admin sets one. Until then, its
  one-hour cache requests are refused.
- The change touches the provider port, every built-in adapter, the WASM interface and the DeepSeek
  component, the price snapshot, the catalog, the ledger's normalized usage, the token check and the
  dashboard's models and request screens.
- `README.md` describes the categories, the one-hour cache, the rate form and the new interface
  version.
