# 0014. Let scripts call the management API with management tokens, and describe the API in an OpenAPI document

- Date: 2026-11-27
- Status: Accepted, not implemented

## Context

This decision lets scripts, CI jobs and other systems use the management API, for example to create
a service principal and issue its virtual keys. Admins issue long-lived management tokens for this.
The gateway also describes the API in an OpenAPI document, a standard format that programs can
read. Signed-in people and tokens can get the document, and the dashboard shows it.

Terms used below:

- A **virtual key** (starts with `mgw_`) is the credential that a client sends with inference
  requests, the requests that the gateway forwards to a provider such as Anthropic or OpenAI. These
  are the **inference routes**, such as `/v1/messages`.
- A **service principal** is a non-human account for inference, for example for a batch job. An
  admin creates it and names a person as its **owner**. When the owner is suspended or loses
  access, the service principal is blocked too.
- A **provider key** is the gateway's own API key for a provider. An admin decides which people and
  service principals may use which provider keys.
- The **access list** (`gw_access`) holds the e-mail addresses that may use the gateway, each with the
  role `user` or `admin`. Admins edit it on the dashboard. Both sign-in modes use it.
- The **command log** (`gw_commands`) keeps the result of each management write, so that a repeated
  command gives the same result. The **audit log** (`gw_audit_events`) keeps who changed what, with
  the values before and after.

The management API is the set of routes under `/api/` (a person's own data and virtual keys) and
`/api/admin/` (admin work). It already covers the service principal flow:

| Step | Call |
|---|---|
| Create a service principal with a human owner | `POST /api/admin/principals` |
| Set the models it may use, and its limits | `PUT /api/admin/principals/{id}/policy` |
| Let it use provider keys | `PUT /api/admin/principals/{id}/provider-credentials` |
| Issue a virtual key for it | `POST /api/keys` with `service_principal_id` |
| Revoke the virtual key | `DELETE /api/keys/{id}` |

Two things are missing:

- **A credential for scripts.** The management API accepts only the credentials of a person who
  signed in: an SSO token (in `sso` mode) or a session token (in `password` mode), together called
  **sign-in tokens** below. Both expire
  within hours, and they need a person at a browser to get a new one. Virtual keys cannot call the
  management API, by design: they are for inference only.
- **A description of the API.** The README describes some routes in prose, and nothing describes
  every route, body and error code. A script author must read the README and the dashboard's code.

Facts that limit the solution:

- Every management write needs an `X-Gateway-Command-ID` header with a UUID. A repeated command with
  the same ID and input gives the same result, so a script can retry safely.
- A browser write that uses a cookie needs a CSRF token (a value from `GET /api/csrf` that proves
  the write came from the dashboard). A browser never adds an `Authorization` header by itself, so a
  credential in that header needs no CSRF token.
- The handlers build most response bodies as JSON values, not as typed Rust structs. So a tool that
  reads Rust types to write an API description would not find the bodies.
- The dashboard loads nothing from external sites (its content security policy forbids it).

## Decision

1. **Management tokens.**
   - Only admins issue them, on the dashboard's new *Admin → Management tokens* screen
     (`/dashboard/admin/tokens`) or with `POST /api/admin/management-tokens`. The body has a name
     (1 to 64 characters), a scope and a validity period.
   - The scope is `read` or `write`. A `read` token can call only `GET` routes. A write call with a
     `read` token gets `403 management_token_not_allowed`. A `write` token can call all routes,
     `GET` and writes, except the routes that rule 2 forbids for all tokens.
   - The validity period is `30_days`, `90_days` or `1_year`. There is no unlimited period.
   - The secret starts with `mgt_` and is shown once. The gateway stores only its SHA-256, as for
     virtual keys and sessions.
   - Each admin can have up to 30 active tokens, that is, tokens that are not revoked and not
     expired. A request for one more token gets `409 management_token_quota_exceeded`.
   - A token acts as the admin who issued it (its owner). At each call, the gateway checks that the
     owner is on the access list, has the admin role and is not suspended, as it does for a
     signed-in person. If any check fails, the call gets `403 access_denied`. The gateway does not
     revoke the token in this case. So the token works again if the owner passes all three checks
     again before the token expires.
   - An unknown, expired or revoked token gets `401 invalid_credentials`.
   - The owner or any other admin can revoke a token (`DELETE /api/admin/management-tokens/{id}`).
     `GET /api/admin/management-tokens` lists all tokens, with their owner, scope, expiry, revoke
     time and last use.
   - The gateway records the last use of each token at most once per minute.
   - The tokens work the same way in `sso` and `password` mode.
2. **How a token is sent, and what it may call.**
   - A token goes in `Authorization: Bearer mgt_...`. A token in a cookie or in the `x-api-key`
     header (where Anthropic clients send an API key) gets
     `401 invalid_credentials`. On an inference route, a valid token gets
     `403 virtual_key_required`, the same answer as for a sign-in token.
   - A call with a token needs no CSRF token. Writes still need `X-Gateway-Command-ID`.
   - A token cannot call the routes that manage tokens (`/api/admin/management-tokens`), so a token
     that leaks cannot make more tokens. It also cannot call three routes that serve a person at a
     browser: `/api/me/password` (change one's password), `/api/csrf` (get a CSRF token) and
     `/auth/login` (go to the dashboard after sign-in). These calls get
     `403 management_token_not_allowed`. The other routes under `/auth/` (password sign-in,
     sign-out, and setting a password from a link) ignore a token.
3. **Audit.** The command log records a token's commands under the token's owner, in the same way
   as the commands that the owner sends from the dashboard. The audit log gets a new column,
   `actor_token_id`, with the token's ID, so it shows which writes came from a token. Issuing and
   revoking a token are audited, with the operations `create_management_token` and
   `revoke_management_token`. The new table `gw_management_tokens` and the new column go into the
   existing migration files, which are edited in place.
4. **The OpenAPI document.**
   - The repository holds one hand-written OpenAPI 3.1 document in JSON,
     `api/management.openapi.json`. The binary embeds it, and the gateway serves it at
     `GET /api/openapi.json` to a signed-in person or a management token of any scope. Anonymous
     requests get `401`.
   - It describes every route under `/api/` and `/api/admin/`: the method, path, path and query
     parameters, request body, response bodies, error codes, the role that may call it (`user` or
     `admin`), the token scope that it needs, and whether it needs `X-Gateway-Command-ID`.
   - It does not describe the inference routes, which follow the providers' own APIs, or the
     browser sign-in routes under `/auth/`.
   - Tests keep the document true:
     - The set of methods and paths in the document equals the router's table of management
       routes, that is, every route under `/api/`, including `/api/admin/`.
     - The management route test (`management_routes_postgres`) sends requests to every management
       route and records the results in `tests/golden/management_routes.txt`. It also checks each
       response body against the document's schema for that route and status.
     - Every error code that the management API can return is in the document.
5. **Dashboard.**
   - *Admin → Management tokens* lists the tokens, issues a token (and shows the secret once), and
     revokes tokens.
   - *API reference* (`/dashboard/api`, for every signed-in person) shows the OpenAPI document: the
     routes by group, and for each route its role, scope, parameters, bodies and error codes, with
     a `curl` example. It is part of the dashboard's own code and loads nothing from outside.
6. **README.** A new section, *Management API*, explains management tokens, links the document and
   the *API reference* screen, and shows the service principal flow as `curl` calls.

## Rejected alternatives

**Give a service principal an admin role and a management key.** Service principals are identities
for inference, with allowed models, limits and virtual keys. A management role would mix two kinds of
identity in one account. A management token instead acts as a person, so the audit log shows which
admin is responsible for each change.

**Let every user issue tokens for their own `/api/` routes.** No current need asks for it. Tokens
for users would need their own rules, for example which `/api/` routes they may call and who may
revoke them. A later decision can add them.

**Tokens with no expiry.** A leaked admin credential that never expires is too large a risk. A
script can get a new token from an admin before the old one expires.

**Generate the document from the code (for example with utoipa, a Rust library that does this).** The handlers build
bodies as JSON values. Generation would need typed structs for every body, which means rewriting
most handlers. A hand-written document with the tests of rule 4 stays true at a lower cost.

**Use Swagger UI or Redoc (common web viewers for OpenAPI documents) on the dashboard.** Loading them from a CDN breaks the content security
policy, and bundling them adds a large library for one screen.

**OAuth client credentials.** In this standard flow, a script gets short-lived tokens from an
authorization server. The gateway has no authorization server of its own: the SSO is an external
system that the gateway does not control, and `password` mode has no authorization server at all.

## Consequences

- The schema changes in the existing migration files, so existing databases must be recreated.
- The command and audit logs gain the new operations, and the audit log gains the `actor_token_id`
  column.
- Each change to a management route must also change the OpenAPI document, or the tests fail.
- The recorded results of the management route test (`tests/golden/management_routes.txt`) grow
  with the new routes.
- `README.md` gets the *Management API* section, and the dashboard gets two screens.
