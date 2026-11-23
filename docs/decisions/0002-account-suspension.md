# 0002. Suspend an account with its own button; remove the security fence

- Date: 2026-11-23
- Status: Dashboard implemented, server part not implemented

## Context

Account suspension was a checkbox inside the policy form. To suspend a person, an admin had to save
the whole policy again, and the reason for the suspension shared one field with the reason for a
policy change.

The same form also had a section for the security fence. The fence is a lock on a suspension: a
suspension whose reason starts with `security_config_fence:` can be lifted only with two reference
values and a confirmation. It existed for one case: a person was removed from the config, and the
suspension had to stay until every instance had restarted. Decision
[0001](0001-access-list-in-the-database.md) removes that case.

## Decision

1. The summary card on the user page has a **Suspend account** button, or **Lift suspension** when
   the account is suspended. The button opens a confirmation window that asks for the reason. The
   window for suspending is a danger window.
2. The confirmation window sends the command directly. The generic "run command" window, which
   shows the request body, is not shown after it.
3. The button for the admin's own account is disabled. The server also rejects the request.
4. The policy form no longer has the suspension checkbox. Saving a policy keeps the current
   suspension state.
5. The dashboard has no fence fields and no text about the fence.
6. The server API does not change for items 1 to 5. Suspension still goes through
   `PUT /api/admin/principals/{id}/policy` with the current grants and limits.
7. The fence is removed from the server together with the implementation of 0001. The handoff
   import also uses the fence, so the two changes must be made in one piece of work.

## Rejected alternatives

**Keep the fence as an option in the new window.** Rejected because 0001 removes the only case that
needed it, and the fields are hard to explain to an admin.

**Remove the fence from the server now.** Rejected because, until 0001 is implemented, removing a
person from the config still depends on a suspension that stays in place, and because the handoff
import uses the same lock.

## Consequences

- Until item 7 is done, the server still creates a fence when a suspension reason starts with
  `security_config_fence:`, and the dashboard cannot lift it. The dashboard shows an error that the
  account is locked. Such an account can be released only through the API.
