# Design: Agent session grouping through `aven agent`

## Goal

Make `aven agent` the interface for grouping tasks and epics into orchestrator sessions. A command belongs under `aven agent` only when it adds session-grouping behavior not provided by the existing CLI. Ordinary task workflows continue to use the commands documented in `CLI.md`; `aven agent` is not a parallel task CLI.

Reuse existing arguments, queries, mutations, serializers, and renderers internally. In particular, session-scoped listing should delegate to the existing list implementation in-process rather than shelling out or duplicating it.

Session membership is optional organizational metadata, not ownership, locking, or a separate task lifecycle. Ordinary commands work with or without membership. This document is a proposal; `CLI.md` remains the reference for shipped commands until implementation.

## Session model

A session is a workspace-scoped grouping of tasks and epics associated with an opaque `SESSION_ID` supplied by the orchestrator. In this proposal, task membership includes epic membership unless stated otherwise.

- Each task or epic belongs to at most one session at a time, but not as a lock. Accept membership changes as instructed.
- Epic membership is independent of its children. Assigning or releasing an epic does not assign or release its children; changing epic relationships does not change session membership.
- Session membership survives status changes, including completion and cancellation. Membership release is explicit.
- A grouping is defined by its members, not a separately created session record. Assigning the first member establishes the grouping; releasing the last leaves it empty. Listing an unused session ID returns an empty result.

## CLI interface

All `aven agent` operational commands emit human-readable text by default. Every operational command accepts `--json` for successful output. Errors remain diagnostic text on stderr in both modes, as in the existing CLI.

### Session commands

| Command                                                        | Behavior                                                              |
| -------------------------------------------------------------- | --------------------------------------------------------------------- |
| `aven agent assign <TASK_REF> --session <SESSION_ID> [--json]` | Add a task or epic to the grouping, replacing any previous membership |
| `aven agent release <TASK_REF> [--json]`                       | Remove a task or epic from its grouping without changing status       |
| `aven agent list --session <SESSION_ID> [OPTIONS]`             | List members of the grouping using the existing task-list options     |

These are the only `aven agent` operational commands in V1. `<TASK_REF>` accepts an ordinary task or epic reference. Creation uses `aven add`, including `--epic` for an epic, followed by `aven agent assign` when membership is wanted. A session selector never establishes an implicit session for subsequent commands.

### Ordinary task workflow

Use `aven list` for unscoped discovery and the existing `aven search`, `aven show`, `aven context`, `aven edit`, `aven delete`, `aven restore`, `aven note`, and `aven note-delete` commands for ordinary task operations. Relationships remain under `aven dep`, `aven related`, and `aven epic`. These commands do not gain agent-prefixed aliases.

Status changes use `aven edit <TASK_REF> --status <STATUS>`, including `active`, `done`, and `canceled`. Deferral, description changes, labels, and ordinary metadata edits likewise use the existing `aven edit` options. Session membership does not add preconditions to these operations.

### Help and global options

`aven agent --help` and `aven help agent` show the three session commands and point to ordinary CLI commands for task workflows. Each session command has full help under `aven agent <COMMAND> --help` and `aven help agent <COMMAND>`. List help reuses ordinary list argument descriptions with session-scoped usage and examples, and documents the required `--session` selector.

Existing global options retain their meaning and parser-supported placement, including `aven --db <PATH> agent <COMMAND>` and `aven --workspace <WORKSPACE> agent <COMMAND>`. The namespace does not select a different database or workspace.

Help identifies this interface as version 1. This version is the only public protocol metadata and is available in help only. There is no `--protocol-version` option or version negotiation.

### Session filtering

`aven agent list --session <SESSION_ID> [OPTIONS]` adds a session-membership constraint to the ordinary list query. `--session` is required; omitting it is an argument parsing error. Use `aven list` when no session constraint is wanted. This proposal adds no `--session` option to ordinary `aven list`: session selection belongs to `aven agent`, while the underlying query and rendering implementation remain shared.

`agent list` accepts the existing list options and inherits their defaults, combination restrictions, availability and deletion scope, ordering, recurrence presentation, result limits, and output formats. This includes `--epics` for listing epic members of the selected session. Maintain one shared filter specification, with the agent adapter supplying the required session constraint.

Add session membership as an AND constraint in the shared task query, before recurrence grouping and result limits. Membership belongs to individual occurrences; grouped output must not imply that every occurrence has the same session. Use the existing `--expand-recurring` option when individual task references are needed. A default session-scoped list is not a complete inventory: ordinary availability filtering still applies, and `--all` does not disable it.

All functionality outside session assignment, release, and scoped listing continues to use the ordinary CLI. The existing `aven prime`, `aven skill`, and skill installation interfaces remain unchanged; their guidance should teach session grouping through `aven agent` alongside ordinary task commands.

### Typical orchestrator session

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

`--ready` helps select work; it does not establish ownership or impose an agent-only precondition on status changes. Completion changes task status without independently verifying the work. Use `aven note` to record a worker report.

## Mutation semantics

Reuse existing task resolution, validation, transactions, mutation, undo, and sync machinery. Command handlers do not write database rows directly. Ordinary task rules, including treatment of deleted tasks, epics, and repeated status changes, apply unchanged.

- Assign sets membership without changing status, replacing any previous session. Assigning the same session is a no-op. A separate transfer command is unnecessary.
- Release clears membership without changing status. Releasing an unassigned task is a no-op; releasing an active task leaves it active.
- `edit --status` uses ordinary status changes for every supported status. It requires neither session membership nor an expected previous status beyond ordinary task validation.
- Ordinary task commands operate without a membership precondition; `aven agent list --session` constrains task selection by membership.
- Recurring tasks are assigned per occurrence. Session membership is never copied into recurrence templates or future occurrences.
- All status changes preserve membership, whether made through ordinary task commands or the TUI.

Membership no-ops use existing no-change handling without new undo entries or sync changes. Status operations inherit ordinary retry behavior.

Session IDs are coordination identifiers, not credentials or authorization tokens. Concurrent unsynchronized metadata edits use existing sync conflict handling rather than agent-specific ownership arbitration.

## Output and errors

Reuse existing CLI serializers and renderers. With `--json`, emit the command result directly as pretty-printed JSON followed by a newline. Keep logs and diagnostic prose off stdout and emit no ANSI styling in JSON.

Do not add response wrappers, protocol identifiers, version fields, success flags, outcome fields, or session summary counts. Existing task and context fields, including their existing dependency and recurrence counts, retain their meanings; there are no new count fields for this interface.

### List output

`aven agent list --session <SESSION_ID> --json` uses the existing list representation for the session-filtered result. An empty result is `[]`.

Add `session_id` to the shared task representation. Use the existing recurrence-group representation without inventing group-level membership. Do not define a separate reduced task schema or a stored or returned readiness flag.

Text output uses the existing task-line format with session membership. Read membership and task-query results from one consistent local snapshot.

### Mutation output

Assign and release use existing mutation rendering conventions and return the post-operation task object directly with `--json`; `session_id` is null after release. Reuse the existing change indicator and retry conventions rather than introducing agent-specific outcome fields or wrappers.

### Context output

`aven context` remains the task-context interface, with its existing arguments, selection, rendering, and JSON structure and no membership precondition. Add `session_id` to the context's task object. Preserve existing names, types, empty-value conventions, and sections, including dependencies, related tasks, notes, conflicts, epics, recurrence, and attachments.

Do not introduce alternate names such as `blockers`, `dependents`, or `related_tasks`, or place the existing context inside a second wrapper. Keep ordinary task detail and context output consistent with the membership display described below.

### Errors and exit codes

Use the existing argument parser and runtime error path. Success, including an unchanged retry, exits with `0`; argument parsing errors use the parser's existing exit behavior; runtime failures exit with `1`. Do not add a separate domain-error exit code or JSON error format.

Use existing diagnostic conventions to explain failures. Malformed metadata and sync conflicts must be distinguishable from missing membership. Reused task validation retains its existing diagnostics; there is no agent-specific readiness error catalog.

## Metadata implementation

Persist one reserved task metadata value under `aven.agent`, encoded as a compact JSON string:

```json
{ "version": 1, "session_id": "run-42" }
```

This version describes the internal storage format, not public response metadata. Absence means no membership. Keep the storage version and session together so sync treats the record as one metadata value. Release removes that value. Store no duplicate membership column, session table, readiness flag, or agent-specific task status.

The current metadata implementation uses string values and rejects public `aven.*` keys. Add an internal, typed membership writer that can create and mutate this reserved key while preserving existing public restrictions. Reuse metadata field identities, size limits, persistence, sync, conflict, undo, and export/import paths. Audit each path: reserved-key validation must allow legitimate restoration and sync without opening generic user writes.

Generic metadata commands and editors may inspect the value but must not set, remove, or rename the reserved field. The semantic core owns encoding and decoding. Imports and conflict resolution must validate supported records before accepting them as usable membership. Preserve unknown storage versions and report an unsupported storage format rather than rewriting or dropping them.

Malformed or conflicted records are not equivalent to unassigned tasks. Operations interpreting membership use the normal error/conflict presentation; ordinary task operations inherit existing validation rather than adding an agent-only check. Session-filtered queries fail if any task in the workspace's selected deletion scope has uninterpretable agent metadata, because Aven cannot prove whether it belongs to the requested session. Enforce this in the shared query implementation when `aven agent list` supplies a session constraint; ordinary unscoped queries do not acquire this session-filter validation. Include affected task references in the diagnostic. Existing conflict tooling remains the explicit repair route.

Expose membership through `session_id`, not by requiring callers to decode reserved metadata. Preserve existing metadata visibility rules on ordinary CLI surfaces.

## Human interface

Show session membership in task detail and context. Add a configurable `agent` table column with header `A`: `·` for valid membership and blank for no membership. Keep the marker independent of task status, including completed tasks. Expose malformed or conflicted metadata through existing error/conflict presentation rather than silently showing an unassigned task.

Include the column in the default layout; allow hiding and reordering through existing table configuration. External metadata and status changes must refresh the TUI without restart. The full session ID belongs in detail, not in the narrow table column.

## Implementation sequence and completion criteria

1. Implement typed session membership in `aven-core`, backed by existing metadata machinery. Complete when assignment, reassignment, release, and no-op behavior pass transactional tests for tasks and epics without changing status or epic relationships. Verify that epic and child membership remain independent.
2. Verify storage lifecycle integration. Complete when restart, sync conflicts, undo, export/import, deletion/restoration, and recurring-occurrence isolation preserve the specified behavior.
3. Implement only `aven agent assign`, `aven agent release`, and `aven agent list`, reusing existing core operations and CLI machinery. Complete when help exposes these commands and directs ordinary task workflows to the existing CLI, unsupported task aliases are rejected, and list requires `--session`. Cover global options, membership mutation output, retries, undo, sync, exit codes, help-only interface version, workspace isolation, and clean JSON stdout. Verify ordinary edits, notes, status changes, and relationship operations still work for members without membership guards or unintended membership changes.
4. Add session filtering to the shared query implementation and reuse ordinary list arguments and execution. Complete when `aven agent list` uses the existing list defaults, validation, rendering, and errors with the additional session constraint. Test session-specific interactions with availability, deletion scope, epic selection, recurrence grouping, limits, workspace isolation, empty groupings, and malformed/conflicted metadata. Verify filtering occurs before recurrence grouping and limits. Keep ordinary filter coverage in the shared list tests rather than duplicating a separate agent filter suite; verify ordinary `aven list` remains unscoped and does not gain a `--session` option.
5. Add TUI membership rendering and refresh coverage. Complete when assignment, release, completion, and external edits are visible without restarting, including custom layouts.
6. Document shipped commands in `CLI.md` and configuration docs; teach `aven prime` and reusable agent guidance the session-grouping workflow alongside ordinary task commands. Complete when examples run against the implementation without raw metadata operations or agent-prefixed task aliases.

## Scope

V1 covers session assignment, release, and scoped listing for tasks and epics. `aven agent` adds only session-grouping behavior; existing CLI commands remain the interface for ordinary task workflows. It does not create a separate agent lifecycle. Scheduling, worker identities, leases, heartbeats, distributed claims, automatic session cleanup, session history, and bulk mutation are separate designs. Avoid adding those concepts to the metadata record until their semantics are specified.
