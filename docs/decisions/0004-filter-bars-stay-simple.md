# 0004. Filter bars get no summary line, reset button or "not applied" marker

- Date: 2026-11-20
- Status: Accepted

## Context

A filter bar on the dashboard applies its conditions only when the user presses the query button.
A proposal tried to make the relation between the conditions and the result more visible.

## Decision

Filter bars stay as they are: fields and one query button. The design canvas shows only the three
states that the code has: loaded, loading and no result.

## Rejected alternatives

All three parts of the proposal were rejected as too much for these screens.

- A summary line above the list, such as "Conditions: user kind = service".
- A reset button in the filter bar and in the empty result.
- A marker on fields that were changed but not yet queried (a dot next to the label, a blue border,
  and a note that the query button applies the change).

## Consequences

- A user who changes a field and does not press the query button sees the old result with the new
  field values. This is accepted.
