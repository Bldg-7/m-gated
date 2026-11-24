# 0005. The catalog is the gateway's own price list; cut-off requests are settled at once; reservations are removed

- Date: 2026-11-24
- Status: Implemented, except the token check and correction (items 11 and 23 to 27)

## Context

This decision makes four changes:

- The catalog becomes the gateway's own price list.
- A request that the client cuts off, or that the provider answers with an error, no longer blocks
  the user. It is settled at once by a rule, or at 0 USD.
- Reservations are removed.
- An optional check compares the gateway's token counts with the provider's usage report.

In this document a principal is a user of the gateway: a person or a service account. A model's
settings, such as its prices and its input limit `max_input_tokens`, are called its profile. The
cost bound of a request is the most it can cost, computed when the request is admitted:
`max_input_tokens` times the highest of the model's input, cache read and cache write prices, plus
the request's `max_tokens` times the output price.

The model catalog was treated as an estimate of what the provider will bill. When the gateway could
not verify a request's final usage, it recorded the cost as `unknown` and rejected all paid calls of
that principal with `unsettled_usage_block` until an admin settled the request by hand.

The most common cause of `unknown` is a stream that the client cuts off. Anthropic sends the output
token count only in the last event of a stream. When a Claude Code user presses Esc, the gateway
never sees that event, so the user is blocked until an admin acts. This happens many times a day.
An HTTP error response from the provider, such as 429 or 529, is also recorded as `unknown` and
blocks the user in the same way.

Anthropic's first stream event reports the input tokens. OpenAI and DeepSeek report all token
counts only in their last event, so when one of their streams is cut off, the gateway knows no
token count at all.

The provider cannot give the cost of one request:

- The Anthropic usage report gives token counts only, in buckets of at least one minute, per API key
  and model. Data appears about 5 minutes after a request ends, sometimes later. It needs an Admin
  API key.
- The Anthropic cost report gives amounts per day only, and cannot be filtered by API key.
- The documentation calls these two endpoints legacy and recommends the Usage and Cost Analytics
  API. That API is in beta, and its data can take 4 to 24 hours to appear. The legacy endpoints
  remain supported.

When a budget was in `reserved` mode, each running request held its cost bound as a reservation.
The dashboard showed the sum of these reservations as "held reservation". The `reserved` mode
admitted a request only if `settled + held + bound ≤ limit`, and `tpm` limits counted the tokens of
running requests. Both needed `input_bound_contract` and, for Anthropic, `internal_count`, which
sends an extra `count_tokens` call before each request. Production builds its profiles from the
database catalog, and those profiles have neither. So in production, a `reserved` budget or a `tpm`
limit made every request fail with `unsupported_strict_policy`, and the held reservation was always
0.

The gateway resells model access inside the company. Its prices do not have to equal the provider's
bill, so its budgets do not have to follow that bill either.

## Decision

### Prices

1. The catalog is the gateway's own price list. Budgets and cost limits use these prices. The
   gateway does not try to match the provider's bill.
2. Admins set the price of each model. The models.dev sync stays as the source of proposed prices.
   There is no margin or multiplier feature.
3. Some models cost more per token when the input is longer than a set length. This stays as it
   is: the catalog sync sets such a model's input limit to that length, so only the base price
   applies.
4. A request keeps the prices that applied when it was admitted. A price change applies only to
   later requests. (Unchanged.)
5. DeepSeek's time-of-day rule (`deepseek_time_unresolved_v1`), which recorded every DeepSeek
   request as `unknown`, is removed. DeepSeek requests are settled at the catalog price.

### Cut-off requests and provider errors

6. In this stage, settlement rules apply to Anthropic only. An Anthropic stream that is cut off
   after the provider's start event is settled at once by a settlement rule. It is never `unknown`,
   and the principal is not blocked. A stream counts as cut off when the client disconnects, the
   gateway shuts down normally and stops the stream itself, the provider stops sending (a read
   timeout or a broken connection), or the provider sends an error event in the stream. In all of
   these cases the gateway still holds the stream's state when it settles. The cases in item 14 are
   not cut-offs.
7. There are two settlement rules. Input tokens are exact in both, because Anthropic's start event
   reports them.
   - `received`: output is the tokens the gateway received before the cut-off.
   - `max_tokens`: output is the request's `max_tokens`.
8. For `received`, the gateway keeps the text, thinking and tool input that it received in memory,
   never in the database. It keeps at most 8 bytes for each token of the request's `max_tokens`.
   After the cut-off, it counts this content with Anthropic's free token counting endpoint, using
   the same provider key. If the content was too large to keep, or the count fails, the request is
   settled by the `max_tokens` rule, and the reason is recorded.
9. Each provider has its own settlement rule. In this stage only Anthropic's rule is used. The
   default is `received`. Admins choose it on the **Settings** screen. It is stored in the
   database, and a request keeps the rule that applied when it was admitted.
10. The documentation and the Settings screen explain the difference between the rules.
    - `received` can count less than the provider bills, for two reasons. When the response has
      thinking, the provider streams only a summary of the thinking but bills all thinking tokens.
      Also, tokens that the provider generated but had not yet sent when the stream was cut off are
      not counted.
    - `max_tokens` never counts less than the provider bills, but it can charge more than was used.
11. Each provider also has a **correction** switch, off by default. When it is on, the gateway
    later replaces the output of a cut-off request with the tokens from the provider's usage report.
    For this it reads the report in one-minute buckets. It corrects a request only when that request
    is the only cut-off request on its provider key in that minute, so the difference between the
    report and the gateway's own count can belong to it alone. The correction is applied to the
    request's original budget period, the request is marked as corrected, and the change is
    audited. The switch can be turned on only for a provider whose usage report the gateway supports
    and only when an Admin API key is configured.
12. The following requests are settled at 0 USD and do not block the principal. The provider's
    documentation does not say whether they are billed. If they were, the token check (item 23)
    shows their tokens as a difference.
    - A stream that was sent but cut off before the provider's start event arrived.
    - A request that is not streamed and was cut off before its response arrived.
    - A request that the provider answered with an HTTP error, such as 400, 429, 500 or 529.
13. OpenAI and DeepSeek streams that are cut off after they start stay `unknown` and still block
    the principal in this stage. Counting their input tokens is the next step.

### Requests with no usage at all

14. In the following cases the gateway cannot know the usage at all. The request is still recorded
    as `unknown`, and its principal is still blocked until an admin settles it:
    - the process stopped unexpectedly during the request (for example it crashed or was killed),
      so no instance holds the request's state,
    - the response reports a model that is not in the model's `actual_models` (the model names the
      provider may report in a response),
    - the response has a usage field the gateway does not know.
15. Requests with a non-standard `service_tier`, `speed` or `inference_geo` are still rejected in
    this stage.

### Reservations

16. Reservations are removed. A request does not hold any amount or tokens while it runs.
17. The `reserved` budget mode is removed, and budgets have no mode. A budget admits a request
    while the settled spend for the current period is below the limit. Requests that are running
    when the limit is reached can push spend past it.
18. `tpm` limits count settled tokens only. A request's tokens are added to the one-minute window in
    which the request is settled. The window in which it was admitted is often already over when a
    long request ends, so tokens added there would never limit anything.
19. `input_bound_contract`, `internal_count` and the error `unsupported_strict_policy` are removed.
    A configuration that still has them fails at startup.
20. The dashboard no longer shows a held reservation, and the policy and key forms no longer ask
    for a budget mode.
21. The gateway still computes the cost bound of each request, for the automatic safety pause: if a
    settled cost is higher than the cost bound, the principal's paid calls are paused until an
    admin releases them.
22. The existing limit `limits.per_principal` stays: on each instance, a principal can have at most
    4 inference requests running at once by default. No new limit is added.

### Token check against the provider

23. The token check and correction are optional. They work only when the environment variable
    `ANTHROPIC_ADMIN_API_KEY` holds an Admin API key. Like `DB_PASSWORD`, it is a secret that is
    set only in the environment. Without it, the gateway does not poll, the token check screen says
    that the check is off, and the correction switch cannot be turned on.
24. One instance at a time polls the legacy usage report (`/v1/organizations/usage_report/messages`)
    every 5 minutes. It reads the last 48 hours in one-hour buckets, grouped by API key and model.
    In this stage only Anthropic is supported.
25. The usage report names API keys by Anthropic's own key ID, not by the key itself. The gateway
    finds the ID of each provider key in the Admin API's list of API keys, which shows the first and
    last characters of every key.
26. The gateway compares tokens per provider key, model, token type and hour. It skips the last 10
    minutes and checks recent hours again, because the provider's data can arrive late.
27. A difference is not an alert and sends no notification. The admin dashboard always shows the
    total difference, priced at the current catalog prices, and a chart of its share by reason:
    - cut-off requests in that hour,
    - use of the provider key outside the gateway (no gateway request in that hour),
    - data not yet arrived (the difference disappears on a later poll),
    - unexplained.

## Rejected alternatives

**Keep blocking on every cut-off request.** Rejected because Claude Code users cut off responses
often, and each one would stop the user until an admin acts.

**Take the cost of a request from the provider's bill.** Rejected because the provider has no
per-request cost, and the cost report is daily and cannot be split by API key.

**Offer only one settlement rule.** Rejected because each rule has a cost. With `received`, the
company pays for thinking tokens that the provider bills but the gateway does not charge to the
user. With `max_tokens`, the user can be charged more than was used. The admin chooses.

**Count the input tokens of OpenAI and DeepSeek in this stage.** Deferred to the next step, because
production uses only Anthropic.

**Keep reservations but hide them.** Rejected because they exist only for a hard budget cap and for
counting the tokens of running requests in `tpm`. With the gateway's own prices, a hard cap is not
needed, and removing reservations also removes the extra `count_tokens` call and the input-bound
configuration.

**Use the Usage and Cost Analytics API.** Rejected because it is in beta and its data can take 4 to
24 hours to appear. The token check needs data within minutes.

**Keep the Admin API key in the database, entered on the dashboard like provider keys.** Rejected
because it is an organization admin credential. Dashboard admins should not be able to replace it,
and it rarely changes.

**Notify admins of every difference.** Rejected because differences are expected (late data,
cut-off requests). A standing summary is more useful than alerts.

**A margin multiplier on synced prices.** Rejected because admins set prices directly.

## Consequences

- A budget can be exceeded by the requests that are running when it is reached.
  `limits.per_principal` bounds how many such requests one principal can have on each instance, and
  `rpm` limits still bound bursts of requests.
- `tpm` limits become usable with profiles from the database catalog. They are soft: one long
  request can add many tokens to a single minute.
- With `received`, the company pays for thinking tokens that are not charged to users. The token
  check shows how large this is.
- OpenAI and DeepSeek cut-offs still block the principal until their input tokens can be counted.
- Whether the company's Anthropic organization can issue an Admin API key is not yet confirmed.
  Two facts can be checked only with a real key: whether a usage bucket is assigned by the start or
  the end of a request, and whether every provider key matches one API key by its hint.
- To change: the ledger (settlement rules, zero-cost cases, removal of reservations, correction),
  the database migrations, the configuration schema, the Settings screen, the usage screens, a new
  token check screen, the Cost ledger and configuration sections of `README.md`, and principle 6 of
  the nano-llm design document ("unknown is not 0 and blocks the principal"), which now applies only
  to the cases in items 13 and 14.
