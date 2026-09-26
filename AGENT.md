# Agent CLI specification

## Commands

Group tasks and epics into orchestrator sessions:

| Command                                                        | Behavior                                              |
| -------------------------------------------------------------- | ----------------------------------------------------- |
| `aven agent assign <TASK_REF> --session <SESSION_ID> [--json]` | Set membership, replacing any previous session        |
| `aven agent release <TASK_REF> [--json]`                       | Remove membership                                     |
| `aven agent list --session <SESSION_ID> [OPTIONS]`             | List session members using ordinary task-list options |

`aven agent` exposes these three subcommands. Creation and task workflows use ordinary commands, including `aven add --epic` for epics. Follow existing CLI conventions for global options, help, output, errors, exit status, and unchanged mutations.

## Membership

“Task” includes epics. `SESSION_ID` is an opaque value supplied by the orchestrator.

- Each task belongs to at most one session. Assign and release change only membership, preserving status and relationships.
- Assigning the current session or releasing an unassigned task leaves storage, undo history, and sync state unchanged.
- Epic and child membership are independent.
- Membership persists through status changes, completion, cancellation, and deletion/restoration until explicitly released.
- Recurring-task membership applies only to the assigned occurrence; templates and future occurrences remain unassigned.
- Sessions are defined by their members. An unused session ID yields an empty list. Session selection is command-local.

## Listing

Reuse the ordinary list implementation in-process, sharing arguments, filters, queries, and rendering. Require `--session` for `aven agent list`; ordinary `aven list` remains unscoped.

Apply membership as an AND constraint before recurrence grouping and limits, reading membership and tasks from one consistent local snapshot. Preserve ordinary availability, deletion scope, ordering, output formats, `--epics`, and option combinations. Availability filtering applies even with `--all`.

Membership is occurrence-specific. Recurrence groups retain their existing schema; `--expand-recurring` exposes individual task references.

## Storage and validation

Persist membership as one reserved metadata value under `aven.agent`, encoded as a compact JSON string:

```json
{ "session_id": "run-42" }
```

Absence means unassigned; release removes the value. An internal typed writer in `aven-core` owns encoding and decoding. Reuse existing task resolution, transactions, metadata limits, persistence, sync, conflict handling, undo, and export/import paths. Generic metadata commands and editors have read-only access to `aven.agent`; restoration, import, sync, and conflict resolution use the reserved-key validation paths.

Validate membership records during import and conflict resolution. Operations interpreting malformed or conflicted membership report affected task references through existing error/conflict presentation; repair uses existing conflict tooling.

Session-filtered queries fail if any task in the selected deletion scope has uninterpretable membership, even when other filters would exclude it. Ordinary unscoped queries retain their existing validation.

## Output

- Add `session_id` to shared task representations, including the task object in `aven context`; use `null` when unassigned and preserve existing metadata visibility rules.
- Assign and release return the post-operation task object directly in JSON and use existing mutation rendering and change indicators in text.
- Lists retain existing representations; an empty JSON list is `[]`.
- Show the full session ID in task detail and context.

## UI

Prefix each assigned task's line item with an AGENT icon, refreshing it when membership changes.

## Example

```sh
aven agent assign APP-7KQ9 --session run-42 --json
aven agent list --session run-42 --ready --expand-recurring --json
aven context APP-7KQ9 --json
aven edit APP-7KQ9 --status active --json
aven edit APP-7KQ9 --status done --json
aven agent release APP-7KQ9 --json
```
