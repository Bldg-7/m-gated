# 0003. Rules for dashboard screens

- Date: 2026-11-13 to 2026-11-23
- Status: Implemented

## Context

The admin screens grew one feature at a time. The same kind of information was shown in different
ways, and several screens asked the admin to type values that only the server knows, such as IDs
and model names. These rules were agreed during the dashboard review and are also in the design
canvas ("관리자 디자인 시스템").

## Decision

**Choosing values**

1. A value that the server knows is chosen from a list, not typed. Users, virtual keys, provider
   keys and models are searchable selects. The model filters read their options from the request
   history: all requests on the admin screens (`GET /api/admin/requests/models`), the user's own
   requests on "My usage" (`GET /api/usage/models`). A model that is no longer approved still
   appears if requests exist for it.
2. No screen opens a record by a typed ID.
3. Models and APIs are chosen in a table with the columns checkbox, Provider, model, API. The
   header checkbox selects all rows and shows the mixed state when only some rows are selected. A
   click on the text of a row selects the row. From 420 px of height the table scrolls inside
   itself and the header row stays in view. This table is used in the policy form, on both pages
   that issue a virtual key, and in the window that edits a virtual key.
4. A choice between two or three fixed values is a segmented control: the settlement decision
   (charged / not charged) and the language switch. The radio input is hidden and the whole segment
   is the click target. Sizes: 40 px high with 14 px text in forms, 36 px with 13 px text in the
   sidebar.

**Pages and windows**

5. A sub-page has a back link above the page title. The link shows the name of the parent page. Its
   accessible name is "Back to …".
6. A section does not repeat the page title. "My virtual keys", "Issue a virtual key" and "Virtual
   key management" have no second heading or lead paragraph below the page title. The key list
   inside the user page keeps its heading, because the page title there is "User detail".
7. A confirmation window for a command that cannot be undone, or that blocks someone, puts the
   first focus on **Cancel**. Enter does not run the command.
8. Row actions in a table are icon buttons with a tooltip. Details open in a window, not in a
   `<details>` element inside the row.

**Status and guidance**

9. A status is shown once. In the user list, "suspended" and "automatic safety pause" each appear
   as one badge, also when the same state is in the list of block reasons.
10. An explanation that belongs to a badge is in the tooltip of the badge, not in a sentence below
    it ("authorization removed" in the key tables). The tooltip opens on hover and on keyboard
    focus, closes with Esc, and is always linked for screen readers.
11. The provider key cell in the key tables shows `Provider · name`, without a label prefix and
    without the last 4 characters. Without a binding it shows "no binding". The last 4 characters
    stay on the provider key screen, in the key detail window and in the issue confirmation.
12. A section for a state that needs an action is shown only while that state exists. The
    "paid requests blocked automatically" section on the user page appears only while the user is
    blocked, with a light red background.
13. A status sentence uses plain words and no internal numbers. The automatic block says when it
    started and why, and links to the request that caused it. It does not show the safety version
    or the request UUID.
14. When an action is not possible, the screen says what to do next, at the place where the action
    would be. When a virtual key cannot be issued, the reason and the next step replace the issue
    button. Text that names another screen links to that screen.
15. A note that applies to a whole table, such as the price unit, is outside the table border.

## Rejected alternatives

See [0004](0004-filter-bars-stay-simple.md) for the filter bar proposals.

## Consequences

- "My usage" can no longer filter by a model name that has no request in the user's history. Such a
  filter always returned an empty result.
- The browser tests that used a free-text model name as a marker now replace the option list of
  `/api/usage/models`. The endpoint itself is covered by the server tests.
