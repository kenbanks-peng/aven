# Design: Agent sessions through `aven agent`

Status: proposed; this document specifies new behavior, not commands already implemented.

## Goal

Give an external orchestrator a small, JSON-first interface for managing a session's tasks. Persist orchestration membership as task metadata, while Aven owns validation, assignment, progress transitions, filtering, and serialization. Agents use semantic commands; they never construct metadata keys or interpret stored protocol records.

The existing task model remains authoritative for status, dependencies, availability, descriptions, and notes. The TUI remains the human view of the same data.

## Session model

A session is a workspace-scoped collection of tasks associated with an opaque session ID supplied by the orchestrator.

- Each task belongs to at most one session at a time.
- A session is a derived view, not another persisted entity. It needs no creation command and can be empty. An empty session is indistinguishable from an ID never used before.
- Accept case-sensitive IDs of 1–128 UTF-8 bytes; reject whitespace and control characters anywhere. Do not trim, lowercase, or otherwise rewrite IDs.
- Require the session ID explicitly on each session operation. There is no ambient current session or dependency on shell state.
- Resolve `--workspace` and `--db` through Aven's existing selection rules. Return the resolved workspace ID in every successful response. Task references must resolve inside that workspace.
- Membership survives status changes, including completion and cancellation. Release is explicit.
- Membership is not an execution lock. Multiple workers using the same session must coordinate externally.

## CLI interface

All `aven agent` operational commands emit JSON by default. No `--json` flag is needed. Help and version output remain normal CLI help.

| Command                                                             | Behavior                                                                           |
| ------------------------------------------------------------------- | ---------------------------------------------------------------------------------- |
| `aven agent protocol`                                               | Describe protocol version, commands, statuses, outcomes, and error codes as JSON   |
| `aven agent session <SESSION> [FILTERS]`                            | Return a session snapshot, counts, and its matching tasks                          |
| `aven agent assign <TASK_REF> --session <SESSION>`                  | Associate an unassigned task with this session                                     |
| `aven agent assign <TASK_REF> --session <NEW> --from-session <OLD>` | Transfer membership only if the task currently belongs to the expected old session |
| `aven agent context <TASK_REF> --session <SESSION>`                 | Return task context after checking membership                                      |
| `aven agent start <TASK_REF> --session <SESSION>`                   | Check membership and readiness, then set task status to `active`                   |
| `aven agent complete <TASK_REF> --session <SESSION>`                | Check membership, then change an `active` task to `done`                           |
| `aven agent release <TASK_REF> --session <SESSION>`                 | Remove matching membership without changing task status                            |

`session` reuses existing task-query semantics for `--ready`, `--blocked`, `--status`, `--project`, and `--label`. Filters combine with session membership using AND. It excludes deleted tasks by default; `--include-deleted` includes them for inspection. Return all matching tasks in v1: no implicit truncation, limit, or pagination.

Use existing task commands for creation, dependency editing, notes, deferral, cancellation, and manual recovery. Agent-specific options live only under `aven agent`. The existing skill installation interface remains unchanged.

### Typical session

```sh
aven agent assign APP-7KQ9 --session run-42
aven agent session run-42 --ready
aven agent context APP-7KQ9 --session run-42
aven agent start APP-7KQ9 --session run-42
# The worker performs the work.
aven agent complete APP-7KQ9 --session run-42
aven agent session run-42
aven agent release APP-7KQ9 --session run-42
```

Aven checks whether a task can start; it does not pick a worker or automatically claim the next task. Successful completion records the worker's report, not independent verification of its work.

## Mutation semantics

Resolve the task, validate membership and task state, and apply each mutation within one local write transaction. Validation failure leaves both metadata and task status unchanged. Reuse existing mutation, undo, and sync machinery rather than writing database rows directly from command handlers.

| Operation | Preconditions and result                                                                                                                                                                   |
| --------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Assign    | Unassigned → `assigned`; already in the requested session → `unchanged`; another session → `session_mismatch`                                                                              |
| Transfer  | Current session equals `--from-session` → `transferred`; already in destination → `unchanged` for safe retries; any other state → `session_mismatch`                                       |
| Start     | Matching membership and `todo` status, plus existing readiness checks → `started`; matching membership and already `active` → `unchanged`; otherwise → `not_ready` or `invalid_transition` |
| Complete  | Matching membership and `active` → `completed`; matching membership and already `done` → `unchanged`; otherwise → `invalid_transition`                                                     |
| Release   | Matching membership → `released`; unassigned → `unchanged`; another session → `session_mismatch`                                                                                           |

- Assignment and transfer do not change status, and may include completed or canceled tasks.
- Validate membership before treating start or complete as an idempotent retry. A stale worker cannot complete another session's task.
- Reject identical source and destination IDs on transfer as `invalid_argument`.
- Deleted tasks are read-only through this interface; restore them using the existing task command before mutation.
- Epic containers can be assigned and inspected, but cannot be started or completed through this worker interface; return `invalid_transition`.
- Recurring tasks are assigned per occurrence. Session membership is never copied into recurrence templates or future occurrences.
- Human edits remain authoritative: normal task status changes preserve membership. After an incompatible manual change, worker commands fail rather than silently restoring their preferred status.
- Release of an active task leaves it active and unassigned. Cleanup is not cancellation or rollback.

All successful mutations return the post-operation task projection and an outcome. Idempotent retries do not create new undo entries or sync changes.

These checks provide local consistency, not distributed exclusivity. Two unsynchronized databases may accept conflicting assignments. Existing sync conflict handling must expose that conflict; agent commands must not silently pick a winner. Session IDs are coordination identifiers, not credentials or authorization tokens.

## JSON protocol v1

Write exactly one UTF-8 JSON object followed by a newline to stdout. Keep progress, logs, and diagnostic prose off stdout. Commands are noninteractive and do not prompt or emit ANSI styling.

Every operational response uses this envelope:

```json
{
  "protocol": "aven.agent",
  "version": 1,
  "ok": true,
  "data": {},
  "error": null
}
```

Success has non-null `data` and null `error`. Failure has null `data` and a structured `error`. Exit codes: `0` success including `unchanged`; `2` invalid invocation or unsupported protocol; `3` domain precondition failure; `1` storage or unexpected failure. Once `aven agent` is recognized, argument parsing errors also use this envelope. Process termination before the CLI can respond is outside this guarantee.

All commands accept `--protocol-version 1`. Unsupported requested versions fail before mutation. Additive response fields are compatible; changing field meanings, types, required fields, or command semantics requires a new protocol version. Callers ignore unknown fields and branch on codes and outcomes, not message text.

### Session snapshot

`aven agent session run-42 --ready` returns data shaped as follows (IDs are illustrative):

```json
{
  "workspace_id": "workspace-id",
  "session_id": "run-42",
  "counts": {
    "total": 3,
    "ready": 1,
    "by_status": {
      "inbox": 0,
      "backlog": 0,
      "todo": 1,
      "active": 1,
      "done": 1,
      "canceled": 0
    }
  },
  "matched_count": 1,
  "tasks": [
    {
      "id": "task-id",
      "ref": "APP-7KQ9",
      "title": "Implement the parser",
      "status": "todo",
      "session_id": "run-42",
      "ready": true,
      "deleted": false
    }
  ]
}
```

Counts cover the entire session within the deletion scope, before optional filters; `matched_count` equals `tasks.length`. Include all six status counts, even when zero. An empty session returns zero counts and an empty array. Order tasks deterministically by task reference, then task ID. Read membership, counts, and readiness from one consistent local snapshot; a later `start` always rechecks readiness.

The task projection shown above is the common minimum for all task responses. `session_id` is null after release. `ready` uses existing readiness rules and is false for deleted tasks and epic containers.

### Mutation and context data

Mutation `data` contains `workspace_id`, `session_id` (the requested session), `outcome`, and `task` (the common projection). Transfer additionally returns `previous_session_id` when a transfer occurred. A retry returning `unchanged` does not invent the original previous session.

Context `data` contains `workspace_id`, `session_id`, `task`, and `context`. The structured `context` contains `description`, `notes`, `blockers`, `dependents`, and `related_tasks`, using existing context selection semantics rather than embedding rendered terminal text. Related-task entries use stable task IDs and references, statuses, and titles. Notes include stable IDs and text. Preserve deterministic ordering. Optional scalars are null; collections are arrays, including when empty. Freeze exact context schemas with contract tests before shipping v1.

### Errors

```json
{
  "protocol": "aven.agent",
  "version": 1,
  "ok": false,
  "data": null,
  "error": {
    "code": "session_mismatch",
    "message": "Task belongs to another session.",
    "details": {
      "task_ref": "APP-7KQ9",
      "expected_session_id": "run-42",
      "actual_session_id": "run-43"
    }
  }
}
```

Stable v1 codes: `invalid_argument`, `unsupported_protocol`, `task_not_found`, `task_deleted`, `session_mismatch`, `not_ready`, `invalid_transition`, `invalid_agent_metadata`, `agent_metadata_conflict`, and `internal_error`. `error.details` is always an object. `not_ready` includes machine-readable reasons derived from existing readiness checks, including blocking task references and availability restrictions where applicable.

## Metadata implementation

Persist one reserved task metadata value under `aven.agent`, encoded as a compact JSON string:

```json
{ "version": 1, "session_id": "run-42" }
```

Absence means no membership. Keep the version and session together so sync treats the record as one metadata value. Release removes that value. Store no duplicate membership column, session table, readiness flag, or agent-specific task status.

The current metadata implementation uses string values and rejects public `aven.*` keys. Add an internal, typed protocol writer that can create and mutate this reserved key while preserving existing public restrictions. Reuse metadata field identities, size limits, persistence, sync, conflict, undo, and export/import paths. Audit each path: reserved-key validation must allow legitimate restoration and sync without opening generic user writes.

Generic metadata commands and editors may inspect the value but must not set, remove, or rename the reserved field. The semantic core owns encoding and decoding. Imports and conflict resolution must validate supported records before accepting them as usable membership; unknown versions remain preserved and report an unsupported protocol rather than being rewritten or dropped.

Malformed or conflicted records are not equivalent to unassigned tasks. Task-targeted commands fail with a structured error. Session snapshots fail if any task in the workspace's selected deletion scope has uninterpretable agent metadata, because Aven cannot prove whether it belongs to the requested session; include affected references in error details. Existing conflict tooling remains the explicit repair route.

No arbitrary metadata fields are included in the agent projection. Agents consume `session_id`, readiness, and normal task fields, not storage details.

## Human interface

Show session membership in task detail and context. Add a configurable `agent` table column with header `A`: `·` for valid membership and blank for no membership. Keep the marker independent of task status, including completed tasks. Expose malformed or conflicted metadata through existing error/conflict presentation rather than silently showing an unassigned task.

Include the column in the default layout; allow hiding and reordering through existing table configuration. External metadata and status changes must refresh the TUI without restart. The full session ID belongs in detail, not in the narrow table column.

## Implementation sequence and completion criteria

1. Implement a typed agent-session module in `aven-core`, backed by metadata. Complete when assignment, guarded transfer/release, readiness checks, and progress transitions pass transactional tests.
2. Verify storage lifecycle integration. Complete when restart, sync conflicts, undo, export/import, deletion/restoration, and recurring-occurrence isolation preserve the specified behavior.
3. Implement `aven agent` as a thin CLI adapter over that module. Complete when JSON schema, exit-code, invalid-invocation, idempotency, workspace-isolation, and stdout-cleanliness contract tests pass.
4. Implement session queries and structured context. Complete when combined filters, deterministic ordering, full-session counts, empty sessions, and malformed/conflicted metadata match this contract.
5. Add TUI membership rendering and refresh coverage. Complete when assignment, release, completion, and external edits are visible without restarting, including custom layouts.
6. Document shipped commands in `CLI.md` and configuration docs; teach `aven prime` and reusable agent guidance the semantic workflow. Complete when examples run against the implementation without raw metadata operations.

## Scope

V1 covers session membership, inspection, guarded progress, and explicit release. Scheduling, worker identities, leases, heartbeats, distributed claims, automatic session cleanup, session history, and bulk mutation are separate designs. Avoid adding those concepts to the metadata record until their semantics are specified.
