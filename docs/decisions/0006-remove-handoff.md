# 0006. Remove the handoff maintenance command

- Date: 2026-11-24
- Status: Implemented

## Context

This decision removes the `handoff` maintenance command.

The gateway's maintenance mode (`m-gated --maintenance`) runs one admin command and exits. One of
its commands is `handoff`. It moves the ledger of one principal (a person or a service account) from
one m-gated database to another, in four steps: fence the principal in the old database, import its
ledger into the new one, then activate it or roll it back. It was built for a migration in which
principals would move one at a time.

The real migration replaced the whole app at once, and the way back is to revert that change. The
command is not described in `README.md`, and nothing uses it.

The command reads and checks the reservation fields that decision
[0005](0005-gateway-prices-and-cut-off-requests.md) removes. Its import also checks the security
fence. That is why item 7 of decision [0002](0002-account-suspension.md) ties the removal of the
fence to the handoff code.

## Decision

1. The `handoff` maintenance command is removed, with its code and tests.
2. The `gw_handoffs` table is dropped. Past `handoff` commands stay in the command log table
   `gw_commands`.
3. The security fence itself stays. Decision 0002 removes it together with decision
   [0001](0001-access-list-in-the-database.md), which moves the access list into the database. That
   plan does not change, but the fence no longer has to change together with the handoff code.

## Rejected alternatives

**Keep the command and update it for 0005.** Rejected because nothing uses it. Keeping it would
mean rewriting its reservation checks and tests for a feature that has no user.

## Consequences

- A principal can no longer be moved to another m-gated database. If that is needed again, it needs
  a new design.
- The work for 0005 becomes smaller, because the handoff format and its tests do not have to change.
