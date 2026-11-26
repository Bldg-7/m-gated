# 0010. Route the management API with axum's method routing and extractors

- Date: 2026-11-26
- Status: Accepted, not implemented

## Context

This decision changes how the gateway chooses the code that handles a management API request. Each
path is registered once, with axum's method routing. The checks that all management requests share
become one middleware. Handlers get their input through extractors. The responses do not change.

### The management API today

The management API is the set of paths that the dashboard calls: `/auth/login` and the paths under
`/api/`. There are 29 paths. The gateway serves them with axum 0.8.8.

The gateway uses some of axum's features. `Router` registers the paths, `fallback` answers unknown
paths, `middleware::from_fn` applies the header limits and the readiness check to every request, and
handlers get shared state through `State`. But the management API does not use axum to choose the
code for a request:

- `src/admin.rs` registers all 29 paths with `any()`, which accepts every method. Four read-only
  paths go to the function `admin::read`. The other 25 paths go to the function
  `management::handle` in `src/admin/management.rs`.
- `management::handle` compares the path as a string to find the methods that the path allows.
  Then it calls `execute`, which compares the path again to choose the work. For Provider keys,
  settings and the model catalog, `execute` calls another function (`provider_credentials`,
  `settings`, `model_catalog`), which compares the path a third time.
- The routes declare path parameters (`/api/keys/{id}`), but no handler uses axum's `Path`
  extractor. The code cuts the ID out of the path with `split('/')` or `rsplit('/')`, sometimes
  with a fixed segment number, for example `segments.get(4)`.
- Four functions read the request body with the same code: `to_bytes` with the body size limit
  (`management_body_bytes`, 64 KiB by default), and a 5 s limit on the time to read the body.

So each path appears in two lists. The first list is the route table in `src/admin.rs`. The second
list is the set of string comparisons, which are spread over five functions. This causes three
problems:

1. The route table and the comparisons can differ. For example, `/api/admin/requests/models` has no
   route of its own. It works only because the route `/api/admin/requests/{id}` also matches it,
   with `models` as the ID.
2. Some comparisons look only at the end of the path, for example `ends_with("/policy")`. The
   result depends on the order of the comparisons, so a new path can match the wrong branch.
3. Each new path must be added in several places. Recent features added paths for Provider keys,
   the model catalog, the budget time zone, the cut-off rule and the token check.

### The shared checks

Every management request goes through these checks today, in this order:

1. **Method.** The method must be one that the path allows. Otherwise the answer is 405.
2. **Request slot.** The gateway limits how many management reads and management writes run at the
   same time. The request takes a slot of its kind (read for `GET`, write for the other methods).
   The slot is kept until the response ends. From this point, the request has a time limit
   (`management_timeout_ms`, 10 s by default). When the limit is reached, the answer is
   `503 db_unavailable`.
3. **Authentication.** The gateway finds the principal: the person or service account that sends
   the request. The way it finds the credential depends on the path:
   - most paths accept a bearer token or the session cookie;
   - `/api/csrf` accepts only the session cookie;
   - `/auth/login` sends the browser to the SSO login page when there is no valid credential.
4. **Admin role.** Paths under `/api/admin/` need an admin.
5. **Key owner.** For `DELETE /api/keys/{id}` only: the key must belong to the principal, or the
   principal must be an admin. Otherwise the answer is 404, so that the key's existence is not shown.
6. **CSRF.** For a write that uses the session cookie, the `Origin` header must be the dashboard's
   origin and the `x-csrf-token` header must hold a valid CSRF token.
7. **Input.** Then the path's own work starts. A write request is a command, for example "issue a
   key" or "change a policy". Each kind of command has a Rust type that holds its fields.
   - A write first needs the `x-gateway-command-id` header: a UUID that the client chooses for each
     command. The gateway records each command in the command log under this ID, so that a retried
     request is not done twice.
   - Then the body is checked, in this order:
     1. `Content-Encoding` is `identity`.
     2. `Content-Length` is in the body size limit.
     3. The whole body arrives in the 5 s limit.
     4. `Content-Type` is JSON.
     5. The JSON scanner accepts the body. It rejects, for example, duplicate keys.
     6. The body is a JSON object.
     7. The fields `service_principal_id`, `display_name`, `management_owner_id`, `fence_release`
        and `execution_stopped_confirmed` do not hold an explicit `null`. These fields may be left
        out, but `null` is not a valid value for them, so the gateway does not treat `null` as a
        missing field.
     8. The body can be read as the command's type.

### Why the code uses `any()`

The management API answers differently from axum's defaults. These facts are from the axum 0.8.8
source:

- A route made with `get()` or with `on(MethodFilter::GET, ...)` also answers `HEAD`. The management
  API rejects `HEAD` with 405.
- When a method is not allowed, axum answers 405 with an empty body and an `Allow` header that it
  makes itself (for example `GET,HEAD`). The management API answers with its own JSON error body
  (`method_not_allowed`) and its own `Allow` header (for example `GET, POST`). axum adds its
  `Allow` header only when the response does not have one.
- axum's `Path` and `Json` extractors reject bad input with their own status codes and plain text.
  The management API answers with its own JSON error body, for example `400 invalid_request` for a
  bad path ID or a bad command body.
- axum has two functions that add a middleware to routes, and they differ for check 1:
  - `Router::route_layer` also wraps the method fallback, which is the code that answers a method
    that the path does not allow. So a middleware added with it runs before the 405 answer.
  - `MethodRouter::route_layer` wraps only the method handlers that the method router has when the
    function is called. It does not wrap the method fallback, and it does not wrap handlers that are
    added later.

### Out of scope

The inference API (`/v1/...` and the other Provider paths) and the probes (`/livez`, `/readyz`) are
not part of this decision. The inference API must answer in each Provider's error format, and it
reads request bodies byte by byte, with limits and zstd decompression. axum's extractors do not help
there.

## Decision

1. Each management path is registered once, in one route table in `src/admin.rs`. Each path and
   method has its own handler function. The string comparisons in `management::handle`, `execute`,
   `provider_credentials`, `settings` and `model_catalog` are removed.
2. Checks 2 to 6 become one middleware. The middleware takes the request slot, starts the time
   limit, authenticates, checks the admin role, the key owner and the CSRF token, and keeps the slot
   until the response ends. It puts the principal into the request, and the handler gets it with an
   extractor. Each route gives the middleware three options: read or write, the way to find the
   credential (check 3), and whether check 5 applies.
3. One helper builds the method router of each path, in this order:
   1. It registers the handlers of the allowed methods (`get`, `post`, `put`, `patch`, `delete`).
   2. It adds the middleware of rule 2 with `MethodRouter::route_layer`, so the middleware wraps
      only those handlers.
   3. It adds a `HEAD` handler and a method fallback. Both return the existing `method_not_allowed`
      response with the gateway's own `Allow` header.

   The middleware does not wrap the `HEAD` handler, because that handler is added after
   `MethodRouter::route_layer` is called. It does not wrap the fallback either. So a wrong method,
   including `HEAD`, still gets 405 before the request slot and authentication, as now. The 405
   answer already has an `Allow` header, so axum does not add its own.
4. Handlers read their input with extractors that answer with the management API's errors:
   - a path ID through a wrapper around `Path`, which answers `400 invalid_request` when the ID is
     not a UUID;
   - the command ID through an extractor for the `x-gateway-command-id` header;
   - the JSON body through one extractor that does the body checks of check 7 in the same order,
     and then makes the command's type;
   - query strings through the existing parsers (`UsageQuery::parse`, `management_query::fields`).
5. The command log records the path of each command. The gateway makes that path from the route's
   template (the path as it is registered, for example `/api/admin/principals/{id}/policy`) and the
   parsed IDs, written in the standard UUID form. So the same command gives the same path even when
   the client writes an ID in capital letters. This is what the code does now with string
   replacement.
6. No response changes: status, error code, body and headers stay the same. Before the code
   changes, new HTTP tests record the current answer of each path in these cases:
   - a method that the path does not allow, including `HEAD`, with the `Allow` header;
   - no credential;
   - a principal that is not an admin, on an admin path;
   - a wrong `Origin` header or CSRF token;
   - a path ID that is not a UUID;
   - a bad body.

   These tests and all existing tests must pass, unchanged, after the change.
7. This decision does not change the inference API, the probes, the redirects from `/` and
   `/dashboard` to the dashboard, the database functions that the handlers call, or the response
   format.

## Rejected alternatives

**Keep the current code.** Rejected because each new path makes the string comparisons longer, and
a mistake there sends a request to the wrong work without an error.

**Use axum's default method routing and 405 answer.** Rejected because a `HEAD` request would get
`200` from the `GET` handler, and a wrong method would get an empty 405 body. The dashboard and the
tests read the error code from the body.

**Use axum's `Path` and `Json` rejections as they are.** Rejected because they answer with other
status codes (for example 422 for JSON that does not match the type) and with plain text, not with
`invalid_request`.

**Use tower-http for the time limit and the body limit.** Rejected because its responses do not
have the management API's error codes. Each case would need an extra layer to change the response,
and the current code for these two limits is short.

**Make the same change in the inference API.** Rejected for the reasons in "Out of scope".

## Consequences

- `src/admin.rs` and `src/admin/management.rs` (together about 1,700 lines) are rewritten. The
  middleware and the extractors are new code next to them. The database functions that do the work
  do not change.
- A new management path needs one line in the route table and one handler. The line shows its
  methods and options. The handler's arguments show its inputs.
- The main risk is a change in the order of the checks. Rule 6 controls this risk: the tests record
  the current answers before the change.
- `README.md` does not change, because the API does not change.
