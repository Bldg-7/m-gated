# Decision records

The top-level `README.md` describes what the gateway and the dashboard do now. This directory
records why a design was chosen and which alternatives were rejected.

Each file is one decision. A file is not edited after the decision is implemented, except for its
status. A later decision that changes an earlier one gets a new file, and the earlier file is marked
`Superseded by NNNN`.

| No. | Decision | Status |
|---|---|---|
| [0001](0001-access-list-in-the-database.md) | Keep the access list in the database, with one fixed admin in the config | Accepted, not implemented |
| [0002](0002-account-suspension.md) | Suspend an account with its own button; remove the security fence | Dashboard implemented, server part not implemented |
| [0003](0003-dashboard-screen-rules.md) | Rules for dashboard screens | Implemented |
| [0004](0004-filter-bars-stay-simple.md) | Filter bars get no summary line, reset button or "not applied" marker | Accepted |
| [0005](0005-gateway-prices-and-cut-off-requests.md) | The catalog is the gateway's own price list; cut-off requests are settled at once; reservations are removed | Implemented |
| [0006](0006-remove-handoff.md) | Remove the handoff maintenance command | Implemented |
| [0007](0007-providers-plug-in-through-one-port.md) | Providers plug in through one port | Implemented |
| [0008](0008-add-typesafe-and-decompress-zstd-requests.md) | Add TypeSafe, and decompress zstd request bodies in the gateway | Implemented |
| [0009](0009-deepseek-adapter-as-wasm-trial.md) | Run the DeepSeek adapter as a WASM component, as a trial | Implemented |

## Format

```markdown
# NNNN. Title

- Date: YYYY-MM-DD
- Status: Accepted, not implemented | Implemented | Superseded by NNNN

## Context
The problem, and the facts that limit the solution.

## Decision
What we do. Use numbered or bulleted rules that a reader can check against the code.

## Rejected alternatives
Each alternative and the reason it was rejected.

## Consequences
The cost of the decision, and the documents and code that must change.
```
