# Design: Agent session grouping through `aven agent`

## Goal and commands

Group tasks and epics into orchestrator sessions using three commands:

| Command                                                        | Behavior                                              |
| -------------------------------------------------------------- | ----------------------------------------------------- |
| `aven agent assign <TASK_REF> --session <SESSION_ID> [--json]` | Set membership, replacing any previous session        |
| `aven agent release <TASK_REF> [--json]`                       | Remove membership                                     |
| `aven agent list --session <SESSION_ID> [OPTIONS]`             | List session members using existing task-list options |

These are the only `aven agent` operational commands. All other task operations use the ordinary CLI and retain their existing behavior, with or without membership. Creation uses `aven add`, including `--epic` for an epic, followed by assignment when wanted. This document is a proposal; `CLI.md` remains the reference for shipped commands until implementation.

Commands follow existing CLI conventions for global options, help, text and JSON output, errors, exit status, and unchanged mutations. Namespace and subcommand help should describe session grouping and point to ordinary commands for task workflows. List help reuses ordinary argument descriptions with session-scoped usage and examples.

## Membership semantics

A session is a workspace-scoped grouping associated with an opaque `SESSION_ID` supplied by the orchestrator. Membership is optional organizational metadata, not ownership, authorization, locking, or a separate task lifecycle. In this proposal, “task” includes epics unless stated otherwise.

- Each task belongs to at most one session. Assign and release change only membership, not status or relationships.
- Assigning the same session or releasing an unassigned task is a no-op, with no new undo entry or sync change.
- Epic membership is independent of its children. Changing epic relationships does not change membership.
- Membership survives all status changes, including completion and cancellation, whether made through the CLI or TUI. Release is explicit.
- Recurring tasks are assigned per occurrence. Membership is never copied into recurrence templates or future occurrences.
- A grouping is defined by its members, not a separate session record. An unused session ID yields an empty list. A session selector applies only to the current command.

Reuse existing task resolution, validation, transactions, mutations, undo, and sync machinery through the semantic core. Concurrent membership edits use existing metadata conflict handling.

## Session-filtered listing

Delegate to the ordinary list implementation in-process, sharing its arguments, filter specification, query, and rendering. The agent adapter supplies a required `--session` constraint; omitting it is an argument parsing error. Ordinary `aven list` remains unscoped and gains no `--session` option.

All ordinary list behavior applies, including option combinations, availability and deletion scope, ordering, recurrence presentation, limits, output formats, and `--epics`. A default session-scoped list is not a complete inventory: availability filtering still applies, and `--all` does not disable it.

Apply membership as an AND constraint in the shared query before recurrence grouping and limits. Membership belongs to individual occurrences; recurrence groups must not imply shared membership. Use `--expand-recurring` when individual task references are needed. Read membership and task-query results from one consistent local snapshot.

## Storage and validation

Persist one reserved task metadata value under `aven.agent`, encoded as a compact JSON string:

```json
{ "version": 1, "session_id": "run-42" }
```

The version describes the internal storage format. Absence means no membership; release removes the value. Keep version and session together so sync treats the record as one metadata value. This metadata is the sole persisted membership representation.

Add an internal, typed membership writer in `aven-core`, which owns encoding and decoding. Reuse existing metadata field identities, size limits, persistence, sync, conflicts, undo, and export/import paths. Reserved-key validation must allow legitimate restoration and sync while preserving public restrictions on `aven.*` keys. Generic metadata commands and editors may inspect the value but must not set, remove, or rename it.

Imports and conflict resolution must validate supported records before accepting them as usable membership. Preserve unknown storage versions and report an unsupported storage format rather than rewriting or dropping them.

Malformed or conflicted records are not equivalent to unassigned tasks. Operations interpreting membership use existing error/conflict presentation, identifying affected task references. Ordinary task operations retain existing validation rather than acquiring an agent-only check. Existing conflict tooling remains the explicit repair route.

**Session-filtered queries fail if any task in the workspace's selected deletion scope has uninterpretable agent metadata**, even if other filters would exclude it: Aven cannot prove whether it belongs to the requested session. Enforce this in the shared query only when a session constraint is supplied. Ordinary unscoped queries do not acquire this validation.

## Output and display

Reuse existing serializers, output schemas, and renderers, adding `session_id` to shared task representations, including the task object in `aven context`. It is null when unassigned. Preserve existing metadata visibility rules; callers should not need to decode reserved metadata.

- Lists retain the existing representation; an empty JSON list is `[]`. Recurrence groups retain their existing schema without group-level membership.
- Assign and release return the post-operation task object directly in JSON, using existing mutation rendering and change indicators in text.
- List text retains the existing task-line format. Show the full session ID in task detail and context.
- Provide an optional `agent` table column with header `A`: `·` for valid membership and blank for no membership, independent of task status. Leave the default layout unchanged; use existing configuration to enable and reorder the column.
- External metadata and status changes must refresh TUI membership displays without restart. Uninterpretable metadata uses the error/conflict presentation above rather than a blank membership marker.

## Typical orchestrator session

```sh
aven agent assign APP-7KQ9 --session run-42 --json
aven agent list --session run-42 --ready --expand-recurring --json
aven context APP-7KQ9 --json
aven edit APP-7KQ9 --status active --json
# The worker performs the work.
aven note APP-7KQ9 "Implemented the change; targeted tests passed." --json
aven edit APP-7KQ9 --status done --json
aven agent list --session run-42 --json
aven agent release APP-7KQ9 --json
```

## Acceptance checklist

- Transactional tests cover membership changes and no-ops for tasks and epics, independent child membership, and ordinary task operations preserving membership.
- Lifecycle tests cover restart, sync conflicts, undo, export/import, deletion/restoration, unknown storage versions, and recurring-occurrence isolation.
- CLI tests cover the three-command boundary, required selector, help, global options, workspace isolation, mutation retries, and existing output/error conventions.
- Shared list tests cover session interactions with availability, deletion scope, epics, recurrence grouping, limits, empty results, and uninterpretable metadata. Keep ordinary filter coverage shared rather than duplicating an agent-specific suite.
- TUI tests cover detail and optional-column rendering, custom layouts, and refresh after assignment, release, completion, and external edits.
- Document shipped commands and column configuration in `CLI.md` and configuration docs. Update `aven prime` and reusable agent guidance with working session-grouping examples, leaving their interfaces and skill installation unchanged.
