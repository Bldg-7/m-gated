# 0005. The catalog is the gateway's own price list; cut-off requests are settled at once; reservations are removed

- Date: 2026-11-24
- Status: Accepted, not implemented

## Context

This decision changes three things. The catalog becomes the gateway's own price list. A request
that the client cuts off is settled at once instead of blocking the user. Reservations are removed.

In this document a principal is a user of the gateway: a person or a service account. The cost bound
of a request is the most it can cost, computed when the request is admitted:
`max_input_tokens × (highest non-output rate) + max_tokens × output rate`.

The model catalog was treated as an estimate of what the provider will bill. When the gateway could
not verify a request's final usage, it recorded the cost as `unknown` and rejected all paid calls of
that principal with `unsettled_usage_block` until an admin settled the request by hand.

The most common cause of `unknown` is a stream that the client cuts off. Anthropic sends the output
token count only in the last event of a stream. When a Claude Code user presses Esc, the gateway
never sees that event, so the user is blocked until an admin acts. This happens many times a day.

The provider cannot give the cost of one request:

- The Anthropic usage report gives token counts only, in buckets of at least one minute, per API key
  and model. Data appears about 5 minutes after a request ends, sometimes later. It needs an Admin
  API key.
- The Anthropic cost report gives amounts per day only, and cannot be filtered by API key.
- The documentation marks both endpoints as legacy and points to the Usage and Cost Analytics API.

While a request ran, its cost bound was counted as a reservation. The dashboard showed the sum of
these reservations as "held reservation". The `reserved` budget mode admitted a request only if
`settled + held + bound ≤ limit`, and `tpm` limits counted reserved tokens. Both needed
`input_bound_contract` and, for Anthropic, `internal_count`, which sends an extra `count_tokens`
call before each request.

The gateway resells model access inside the company. Its prices do not have to equal the provider's
bill, so its budgets do not have to follow that bill either.

## Decision

### Prices

1. The catalog is the gateway's own price list. Budgets and cost limits use these prices. The
   gateway does not try to match the provider's bill.
2. Admins set the price of each model. The models.dev sync stays as the source of proposed prices.
   There is no margin or multiplier feature.
3. Price tiers by input length stay.
4. A request keeps the prices that applied when it was admitted. A price change applies only to
   later requests. (Unchanged.)

### Cut-off requests

5. A request whose stream is cut off after the provider's start event is settled at once by a
   settlement rule. It is never `unknown`, and the principal is not blocked. A request that was sent
   but cut off before the start event is settled at 0 USD and does not block the principal either.
   The provider's documentation does not say whether such a request is billed. If it was billed,
   the token check (item 21) shows the tokens as a difference caused by cut-off requests.
6. There are two settlement rules. Input tokens are exact in both, because the start event reports
   them.
   - `received`: output is the tokens the gateway received before the cut-off.
   - `max_tokens`: output is the request's `max_tokens`.
7. Each provider has its own settlement rule. The default is `received`. Admins choose it on the
   **Settings** screen. It is stored in the database, and a request keeps the rule that applied when
   it was admitted.
8. The documentation and the Settings screen explain the difference between the rules.
   - `received` can count less than the provider bills, for two reasons. When the response has
     thinking, the provider streams only a summary of the thinking but bills all thinking tokens.
     Also, tokens that the provider generated but had not yet sent when the stream was cut off are
     not counted.
   - `max_tokens` never counts less than the provider bills, but it can charge more than was used.
9. Each provider also has a **correction** switch, off by default. When it is on, the gateway later
   replaces the output of a cut-off request with the tokens from the provider's usage report. It does
   this only when the difference between the report and the gateway's own count, for that provider
   key and time, can belong to that request alone. The correction is applied to the request's
   original budget period, the request is marked as corrected, and the change is audited. The
   switch can be turned on only for a provider whose usage report the gateway supports and only
   when an Admin API key is configured.

### Requests with no usage at all

10. In the following cases the gateway cannot know the usage at all. The request is still recorded
    as `unknown`, and its principal is still blocked until an admin settles it:
    - the process stopped during the request,
    - the response reports a model that is not in the model's `actual_models` (the model names the
      provider may report in a response),
    - the response has a usage field the gateway does not know.
11. Requests with a non-standard `service_tier`, `speed` or `inference_geo` are still rejected in
    this stage.

### Reservations

12. Reservations are removed. A request does not hold any amount or tokens while it runs.
13. The `reserved` budget mode is removed. A budget admits a request while the settled spend for
    the current period is below the limit. Requests that are running when the limit is reached can
    push spend past it.
14. `tpm` limits count settled tokens only.
15. `input_bound_contract`, `internal_count` and the error `unsupported_strict_policy` are removed.
    A configuration that still has them fails at startup.
16. The dashboard no longer shows a held reservation.
17. The gateway still computes the cost bound of each request. The `max_tokens` rule uses it. The
    automatic safety pause also stays: if a settled cost is higher than the cost bound, the
    principal's paid calls are paused until an admin releases them.
18. There is no per-principal concurrency limit in this stage.

### Token check against the provider

19. When an Admin API key is configured, the gateway polls the provider's usage report every 5
    minutes. In this stage only Anthropic is supported.
20. It compares tokens per provider key, model, token type and hour. It skips the last 10 minutes
    and checks recent hours again, because the provider's data can arrive late.
21. A difference is not an alert and sends no notification. The admin dashboard always shows the
    total difference and a chart of its share by reason:
    - cut-off requests in that hour,
    - use of the provider key outside the gateway (no gateway request in that hour),
    - data not yet arrived (the difference disappears on a later poll),
    - unexplained.

## Rejected alternatives

**Keep blocking on every cut-off request.** Rejected because Claude Code users cut off responses
often, and each one would stop the user until an admin acts.

**Take the cost of a request from the provider's bill.** Rejected because the provider has no
per-request cost, and the cost report is daily and cannot be split by API key.

**Offer only one settlement rule.** Rejected because each rule has a cost: `received` lets the
company absorb unbilled thinking tokens, and `max_tokens` can overcharge the user. The admin
chooses.

**Keep reservations but hide them.** Rejected because they exist only for a hard budget cap and for
counting the tokens of running requests in `tpm`. With the gateway's own prices, a hard cap is not
needed, and removing reservations also removes the extra `count_tokens` call and the input-bound
configuration.

**Notify admins of every difference.** Rejected because differences are expected (late data,
cut-off requests). A standing summary is more useful than alerts.

**A margin multiplier on synced prices.** Rejected because admins set prices directly.

## Consequences

- A budget can be exceeded by the requests that are running when it is reached. `rpm` limits still
  bound bursts of requests.
- With `received`, the company pays for thinking tokens that are not charged to users. The token
  check shows how large this is.
- Providers do not report a token count for a partial response. The method that `received` uses to
  count the received output (for example, the provider's token counting endpoint or a published
  tokenizer) is chosen for each provider during implementation.
- Whether the company's Anthropic organization can issue an Admin API key is not yet confirmed.
  Without it, correction and the token check cannot be used.
- The endpoint for the usage report (legacy or the Usage and Cost Analytics API) is chosen during
  implementation.
- To change: the ledger (settlement rules, removal of reservations, correction), the database
  migrations, the configuration schema, the Settings screen, the usage screens, a new token check
  screen, the Cost ledger and configuration sections of `README.md`, and principle 6 of the
  nano-llm design document ("unknown is not 0 and blocks the principal"), which now applies only to
  the cases in item 10.
