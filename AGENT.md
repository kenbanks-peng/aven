# Design: Agent sessions through `aven agent`

## Goal

Give an external orchestrator a small interface for managing a session's tasks. Use the existing CLI conventions for text, JSON, errors, and exit codes. Persist session membership as task metadata.

## Session model

A session is a workspace-scoped collection of tasks associated with an opaque session ID supplied by the orchestrator.

- Each task belongs to at most one session at a time.
- Session membership survives status changes, including completion and cancellation. Release is explicit.

## CLI interface

All `aven agent` operational commands emit human-readable text by default. Every operational command accepts `--json` for successful output. Errors remain diagnostic text on stderr in both modes, as in the existing CLI.

| Command | Behavior |
| --- | --- |
| `aven agent --help` or `aven help agent` | Show interface version, commands, and usage |
| `aven agent session <SESSION> [FILTERS]` | List matching session tasks |
| `aven agent assign <TASK_REF> --session <SESSION>` | Associate an unassigned task with this session |
| `aven agent assign <TASK_REF> --session <NEW> --from-session <OLD>` | Transfer membership only if the task belongs to the expected old session |
| `aven agent context <TASK_REF> --session <SESSION>` | Show task context after checking membership |
| `aven agent start <TASK_REF> --session <SESSION>` | Check membership and readiness, then set task status to `active` |
| `aven agent complete <TASK_REF> --session <SESSION>` | Check membership, then change an `active` task to `done` |
| `aven agent release <TASK_REF> --session <SESSION>` | Remove matching membership without changing task status |

Help identifies this interface as version 1. This version is the only public protocol metadata and is available in help only. There is no `--protocol-version` option or version negotiation.

`session` supports `--ready`, `--blocked`, `--status`, `--project`, and `--label`, with the existing task-query meanings and combination restrictions. Apply session membership as an additional AND constraint. Exclude deleted tasks by default; `--all` includes them for inspection. Reject `--ready` together with `--blocked`, and reject either filter together with `--all`, as `aven list` does.

Session inspection includes deferred tasks by default, so assigned work remains visible. This is an explicit difference from `aven list`, which normally filters for availability. `--all` changes only deletion scope. `--ready` still requires availability and the existing dependency checks. The `todo` status alone does not establish readiness.

The v1 session interface supports only the filters listed above. Return all matching tasks, with no implicit truncation, limit, or pagination. Use the ordinary `aven list` ordering: updated time, descending. Keep recurring occurrences as individual tasks because membership applies to each occurrence.

Use existing task commands for creation, dependency editing, notes, deferral, cancellation, and manual recovery. Agent-specific options live only under `aven agent`. The existing skill installation interface remains unchanged.

### Typical orchestrator session

```sh
aven agent assign APP-7KQ9 --session run-42 --json
aven agent session run-42 --ready --json
aven agent context APP-7KQ9 --session run-42 --json
aven agent start APP-7KQ9 --session run-42 --json
# The worker performs the work.
aven agent complete APP-7KQ9 --session run-42 --json
aven agent session run-42 --json
aven agent release APP-7KQ9 --session run-42 --json
```

Aven checks whether a task can start; it does not pick a worker or automatically claim the next task. Completion changes the task status; it does not independently verify the work. Use the existing note command to record a worker report.

## Mutation semantics

Resolve the task, validate membership and task state, and apply each mutation within one local write transaction. Validation failure leaves both metadata and task status unchanged. Reuse existing mutation, undo, and sync machinery rather than writing database rows directly from command handlers.

| Operation | Preconditions and result |
| --- | --- |
| Assign | Unassigned: assign membership. Already in the requested session: no change. Another session: fail. |
| Transfer | Current session equals `--from-session`: transfer membership. Already in destination: no change, for safe retries. Any other state: fail. |
| Start | Matching membership and ready `todo` task: set `active`. Matching membership and already `active`: no change. Otherwise: fail. |
| Complete | Matching membership and `active`: set `done`. Matching membership and already `done`: no change. Otherwise: fail. |
| Release | Matching membership: remove membership. Unassigned: no change. Another session: fail. |

- Assignment and transfer do not change status, and may include completed or canceled tasks.
- Validate membership before treating start or complete as an idempotent retry. A stale worker cannot complete another session's task.
- Reject identical source and destination IDs on transfer.
- Deleted tasks are read-only through this interface. Restore them using the existing task command before mutation.
- Epic containers can be assigned and inspected, but cannot be started or completed through this worker interface.
- Recurring tasks are assigned per occurrence. Session membership is never copied into recurrence templates or future occurrences.
- Human edits remain authoritative: normal task status changes preserve membership. After an incompatible manual change, worker commands fail rather than restore their preferred status.
- Release of an active task leaves it active and unassigned. Cleanup is not cancellation or rollback.

Idempotent retries succeed without new undo entries or sync changes.

These checks provide local consistency, not distributed exclusivity. Two unsynchronized databases may accept conflicting assignments. Existing sync conflict handling must expose that conflict; agent commands must not silently pick a winner. Session IDs are coordination identifiers, not credentials or authorization tokens.

## Output and errors

Reuse existing CLI serializers and renderers. With `--json`, emit the command result directly as pretty-printed JSON followed by a newline. Keep logs and diagnostic prose off stdout and emit no ANSI styling in JSON.

Do not add response wrappers, protocol identifiers, version fields, success flags, outcome fields, or session summary counts. Existing task and context fields, including their existing dependency and recurrence counts, retain their meanings; there are no new count fields for this interface.

### Session output

`aven agent session <SESSION> --json` returns an array of task objects, as `aven list --json` does. An empty result is `[]`.

Reuse the existing task-list JSON representation and add `session_id` as a task field. Do not define a separate reduced task schema or add a stored or returned readiness flag. Callers use `--ready` to query readiness, and `start` always checks readiness again.

Text output uses the existing task-line format with session membership. Read membership and task-query results from one consistent local snapshot.

### Mutation output

With `--json`, return the post-operation task object directly, using the same representation as session results. `session_id` is null after release. Do not wrap the task in an operation report or repeat the requested session, workspace, or previous session outside the task.

Text output follows existing mutation commands: an operation summary with the task reference, `changed=true/false`, and relevant task fields. JSON retries return the current task object without a separate outcome marker.

### Context output

After checking membership, `aven agent context` uses the existing `aven context` selection, rendering, and JSON structure. Add `session_id` to the context's task object. Preserve existing names, types, empty-value conventions, and sections, including dependencies, related tasks, notes, conflicts, epics, recurrence, and attachments.

Do not introduce alternate names such as `blockers`, `dependents`, or `related_tasks`, or place the existing context inside a second wrapper. Keep ordinary task detail and context output consistent with the membership display described below.

### Errors and exit codes

Use the existing argument parser and runtime error path. Success, including an unchanged retry, exits with `0`; argument parsing errors use the parser's existing exit behavior; runtime failures, including membership and readiness failures, exit with `1`. Do not add a separate domain-error exit code or JSON error format.

Use existing diagnostic conventions to explain failures. Membership errors identify the task and expected and actual sessions. Readiness errors identify blocking tasks or availability restrictions where applicable. Malformed metadata and sync conflicts must be distinguishable from missing membership. The help text describes these preconditions without defining a separate catalog of protocol error codes.

## Metadata implementation

Persist one reserved task metadata value under `aven.agent`, encoded as a compact JSON string:

```json
{ "version": 1, "session_id": "run-42" }
```

This version describes the internal storage format, not public response metadata. Absence means no membership. Keep the storage version and session together so sync treats the record as one metadata value. Release removes that value. Store no duplicate membership column, session table, readiness flag, or agent-specific task status.

The current metadata implementation uses string values and rejects public `aven.*` keys. Add an internal, typed membership writer that can create and mutate this reserved key while preserving existing public restrictions. Reuse metadata field identities, size limits, persistence, sync, conflict, undo, and export/import paths. Audit each path: reserved-key validation must allow legitimate restoration and sync without opening generic user writes.

Generic metadata commands and editors may inspect the value but must not set, remove, or rename the reserved field. The semantic core owns encoding and decoding. Imports and conflict resolution must validate supported records before accepting them as usable membership. Preserve unknown storage versions and report an unsupported storage format rather than rewriting or dropping them.

Malformed or conflicted records are not equivalent to unassigned tasks. Task-targeted commands fail through the normal CLI error path. Session queries fail if any task in the workspace's selected deletion scope has uninterpretable agent metadata, because Aven cannot prove whether it belongs to the requested session. Include affected task references in the diagnostic. Existing conflict tooling remains the explicit repair route.

Expose membership through `session_id`, not by requiring callers to decode reserved metadata. Preserve existing metadata visibility rules on ordinary CLI surfaces.

## Human interface

Show session membership in task detail and context. Add a configurable `agent` table column with header `A`: `·` for valid membership and blank for no membership. Keep the marker independent of task status, including completed tasks. Expose malformed or conflicted metadata through existing error/conflict presentation rather than silently showing an unassigned task.

Include the column in the default layout; allow hiding and reordering through existing table configuration. External metadata and status changes must refresh the TUI without restart. The full session ID belongs in detail, not in the narrow table column.

## Implementation sequence and completion criteria

1. Implement a typed agent-session module in `aven-core`, backed by metadata. Complete when assignment, guarded transfer/release, readiness checks, and progress transitions pass transactional tests.
2. Verify storage lifecycle integration. Complete when restart, sync conflicts, undo, export/import, deletion/restoration, and recurring-occurrence isolation preserve the specified behavior.
3. Implement `aven agent` as a thin CLI adapter over that module. Complete when tests cover text output, direct JSON results, existing stderr and exit-code behavior, help-only interface version, idempotency, workspace isolation, and clean JSON stdout.
4. Implement session queries and reuse structured context. Complete when tests cover combined filters, deferred-task visibility, existing list ordering, individual recurring occurrences, empty arrays, malformed/conflicted metadata, and reuse of existing task and context schemas without new summary fields.
5. Add TUI membership rendering and refresh coverage. Complete when assignment, release, completion, and external edits are visible without restarting, including custom layouts.
6. Document shipped commands in `CLI.md` and configuration docs; teach `aven prime` and reusable agent guidance the semantic workflow. Complete when examples run against the implementation without raw metadata operations.

## Scope

V1 covers session membership, inspection, guarded progress, and explicit release. Scheduling, worker identities, leases, heartbeats, distributed claims, automatic session cleanup, session history, and bulk mutation are separate designs. Avoid adding those concepts to the metadata record until their semantics are specified.
