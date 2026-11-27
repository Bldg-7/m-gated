# 0001. Keep the access list in the database, with one fixed admin in the config

- Date: 2026-11-23
- Status: Implemented

## Context

Two places decide who can use the gateway.

- The config file has `allowed_emails` and `admin_emails`. A change to these lists applies only
  after every instance has restarted with the new config.
- The database has policies, keys and account suspension. A change to these applies on the next
  request.

Because the two places apply changes at different speeds, removing a person needs two steps. The
admin removes the person from the config, and also suspends the account in the database until all
instances have restarted. A special lock (the security fence, see
[0002](0002-account-suspension.md)) was added only to stop another admin from lifting that
suspension too early.

## Decision

The database is the only source for access. The config keeps one admin e-mail address.

1. Remove `allowed_emails` from the config. An admin adds and removes users on the dashboard. The
   change applies on the next request. A user who signs in for the first time starts with an empty
   policy (deny-all), as now.
2. Replace the `admin_emails` list with a single `admin_email` (environment variable
   `ADMIN_EMAILS` becomes `ADMIN_EMAIL`). All other admins are managed in the database.
3. Add a table keyed by e-mail address (e-mail, role, added by, added at). An admin can add a
   person before that person has signed in. At sign-in, the gateway matches the verified e-mail
   address against this table.
4. The person named by `admin_email` is the fixed admin:
   - At each start, the gateway adds `admin_email` to the database as an admin if it is not there.
   - The dashboard shows this person as "config admin". The admin role of this person cannot be
     removed. Another admin can still suspend this person in an emergency.
   - If `admin_email` changes and the gateway restarts, the new person becomes the fixed admin. The
     earlier person stays in the database as a normal admin and can then be removed on the
     dashboard.
   - If all admins are locked out, the operator changes `admin_email` to a person who can sign in
     and restarts. There is no other recovery tool.
5. These rules stay: an admin cannot suspend or demote their own account, and the last active admin
   cannot be suspended or demoted.

The change is breaking. `allowed_emails` and `admin_emails` are removed without a migration path,
and the commit uses `feat!:`.

## Rejected alternatives

**Bootstrap only.** `admin_email` is used only when the database has no active admin. The config
admin then looks like every other admin on the dashboard.

Rejected because lock-out recovery needs a separate switch. When the SSO accounts of all admins are
deleted, the database still shows them as active, so the gateway cannot detect the lock-out
by itself.

## Consequences

- The rule "an admin cannot create an admin" no longer holds. If one admin session is stolen, the
  attacker can create more admins. Every admin addition is written to the audit log.
- Removing a person no longer needs the security fence. See [0002](0002-account-suspension.md).
- Documents to change with the code: the Configuration reference table, the environment variable
  table, "First use" in Quick start, "Changing access" in Operations, and `config.example.yaml`.
