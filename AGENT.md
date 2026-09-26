# Design: First-class agent CLI through `aven agent`

## Goal

Give agents and external orchestrators a first-class `aven agent` entry point for task workflows, with the same command vocabulary and argument shapes as the ordinary CLI documented in `CLI.md`. Thin adapters are an implementation detail: agents should be able to discover and perform the supported workflow through `aven agent --help` without switching command namespaces. Reuse existing arguments, queries, mutations, serializers, and renderers rather than defining a parallel CLI.

Session membership is optional organizational metadata, not ownership, locking, or a separate task lifecycle. Agents can use the interface with or without an orchestrator session. This document is a proposal; `CLI.md` remains the reference for shipped commands until implementation.

## Session model

A session is a workspace-scoped collection of tasks associated with an opaque session ID supplied by the orchestrator.

- Each task belongs to at most one session at a time, but not as a lock. Accept membership changes as instructed.
- Session membership survives status changes, including completion and cancellation. Membership release is explicit.

## CLI interface

All `aven agent` operational commands emit human-readable text by default. Every operational command accepts `--json` for successful output. Errors remain diagnostic text on stderr in both modes, as in the existing CLI.

### Shared task commands

For each command below, inserting `agent` after `aven` preserves the command's arguments, options, defaults, validation, side effects, and output. `[OPTIONS]` means the corresponding ordinary command's options, not a reduced agent-specific set. Share argument definitions and execution in-process; do not shell out.

| Agent command                                           | Ordinary CLI equivalent                                                         |
| ------------------------------------------------------- | ------------------------------------------------------------------------------- |
| `aven agent add <TITLE> [OPTIONS]`                      | `aven add <TITLE> [OPTIONS]`, including natural-language and recurring creation |
| `aven agent list [OPTIONS]`                             | `aven list [OPTIONS]`                                                           |
| `aven agent search <QUERY>... [OPTIONS]`                | `aven search <QUERY>... [OPTIONS]`                                              |
| `aven agent show <TASK_REF> [OPTIONS]`                  | `aven show <TASK_REF> [OPTIONS]`                                                |
| `aven agent context <TASK_REF> [OPTIONS]`               | `aven context <TASK_REF> [OPTIONS]`                                             |
| `aven agent edit <TASK_REF> [OPTIONS]`                  | `aven edit <TASK_REF> [OPTIONS]`                                                |
| `aven agent delete <TASK_REF> [OPTIONS]`                | `aven delete <TASK_REF> [OPTIONS]`                                              |
| `aven agent restore <TASK_REF> [OPTIONS]`               | `aven restore <TASK_REF> [OPTIONS]`                                             |
| `aven agent note <TASK_REF> [TEXT] [OPTIONS]`           | `aven note <TASK_REF> [TEXT] [OPTIONS]`, including `--file` and `--stdin`       |
| `aven agent note-delete <TASK_REF> <NOTE_ID> [OPTIONS]` | `aven note-delete <TASK_REF> <NOTE_ID> [OPTIONS]`                               |
| `aven agent dep <SUBCOMMAND> [ARGS] [OPTIONS]`          | `aven dep <SUBCOMMAND> [ARGS] [OPTIONS]`                                        |
| `aven agent related <SUBCOMMAND> [ARGS] [OPTIONS]`      | `aven related <SUBCOMMAND> [ARGS] [OPTIONS]`                                    |
| `aven agent epic <SUBCOMMAND> [ARGS] [OPTIONS]`         | `aven epic <SUBCOMMAND> [ARGS] [OPTIONS]`                                       |

Status changes use `aven agent edit <TASK_REF> --status <STATUS>`, including `active`, `done`, and `canceled`. Use the existing status vocabulary rather than introducing `start` and `complete` commands. Deferral, description changes, labels, and ordinary metadata edits likewise use the existing `edit` options.

### Session membership commands

| Command                                                     | Behavior                                       |
| ----------------------------------------------------------- | ---------------------------------------------- |
| `aven agent assign <TASK_REF> --session <SESSION> [--json]` | Set membership, replacing any previous session |
| `aven agent release <TASK_REF> [--json]`                    | Clear membership without changing task status  |

These are the only new task operations. Creation does not implicitly assign membership; use `assign` with the created task's reference. A session selector filters a query; it never establishes an implicit session for subsequent commands.

### Help and global options

`aven agent --help` and `aven help agent` show the supported commands and usage. Each shared command has full help under `aven agent <COMMAND> --help` and `aven help agent <COMMAND>`, using the shared argument descriptions and agent-prefixed usage and examples. Agents should not need ordinary-command help to discover supported options.

Existing global options retain their meaning and parser-supported placement, including `aven --db <PATH> agent <COMMAND>` and `aven --workspace <WORKSPACE> agent <COMMAND>`. The namespace does not select a different database or workspace.

Help identifies this interface as version 1. This version is the only public protocol metadata and is available in help only. There is no `--protocol-version` option or version negotiation.

### Session filtering

`aven agent list --session <SESSION> [OPTIONS]` is equivalent to `aven list --session <SESSION> [OPTIONS]`. The optional `--session` flag is shared by both commands; neither takes a positional session. Without it, `aven agent list` is an ordinary workspace task list, not an error or an implicit session lookup.

`agent list` accepts the existing list options and inherits their defaults, combination restrictions, availability and deletion scope, ordering, recurrence presentation, result limits, and output formats. Maintain one shared filter specification.

Add session membership as an AND constraint in the shared task query, before recurrence grouping and result limits. Membership belongs to individual occurrences; grouped output must not imply that every occurrence has the same session. Use the existing `--expand-recurring` option when individual task references are needed. A default session-scoped list is not a complete inventory: ordinary availability filtering still applies, and `--all` does not disable it.

The shared commands cover creation, discovery, inspection, editing, relationships, reporting, and deletion/restoration within `aven agent`. Operations outside this task workflow continue to use the ordinary CLI, including recurring-series management, guarded text operations, attachments, bulk updates, workspace/project administration, sync/conflict repair, and data safety. The existing `aven prime`, `aven skill`, and skill installation interfaces remain unchanged; their guidance should teach the agent-prefixed task workflow.

### Typical orchestrator session

```sh
aven agent assign APP-7KQ9 --session run-42 --json
aven agent list --session run-42 --ready --expand-recurring --json
aven agent context APP-7KQ9 --json
aven agent edit APP-7KQ9 --status active --json
# The worker performs the work.
aven agent note APP-7KQ9 "Implemented the change; targeted tests passed." --json
aven agent edit APP-7KQ9 --status done --json
aven agent list --session run-42 --json
aven agent release APP-7KQ9 --json
```

`--ready` helps select work; it does not establish ownership or impose an agent-only precondition on status changes. Completion changes task status without independently verifying the work. Use `aven agent note` to record a worker report.

## Mutation semantics

Reuse existing task resolution, validation, transactions, mutation, undo, and sync machinery. Command handlers do not write database rows directly. Ordinary task rules, including treatment of deleted tasks, epics, and repeated status changes, apply unchanged.

- Assign sets membership without changing status, replacing any previous session. Assigning the same session is a no-op. A separate transfer command is unnecessary.
- Release clears membership without changing status. Releasing an unassigned task is a no-op; releasing an active task leaves it active.
- `edit --status` uses ordinary status changes for every supported status. It requires neither session membership nor an expected previous status beyond ordinary task validation.
- Shared task commands operate without a membership precondition; only an explicit `list --session` constrains task selection by membership.
- Recurring tasks are assigned per occurrence. Session membership is never copied into recurrence templates or future occurrences.
- All status changes preserve membership, whether made through `aven agent`, ordinary task commands, or the TUI.

Membership no-ops use existing no-change handling without new undo entries or sync changes. Status operations inherit ordinary retry behavior.

Session IDs are coordination identifiers, not credentials or authorization tokens. Concurrent unsynchronized metadata edits use existing sync conflict handling rather than agent-specific ownership arbitration.

## Output and errors

Reuse existing CLI serializers and renderers. With `--json`, emit the command result directly as pretty-printed JSON followed by a newline. Keep logs and diagnostic prose off stdout and emit no ANSI styling in JSON.

Do not add response wrappers, protocol identifiers, version fields, success flags, outcome fields, or session summary counts. Existing task and context fields, including their existing dependency and recurrence counts, retain their meanings; there are no new count fields for this interface.

### List output

`aven agent list --session <SESSION> --json` and `aven list --session <SESSION> --json` return identical output using the existing list representation. An empty result is `[]`.

Add `session_id` to the shared task representation. Use the existing recurrence-group representation without inventing group-level membership. Do not define a separate reduced task schema or a stored or returned readiness flag.

Text output uses the existing task-line format with session membership. Read membership and task-query results from one consistent local snapshot.

### Mutation output

Shared mutation commands, including `edit --status`, reuse their ordinary command's text and JSON output unchanged. Assign and release use existing mutation rendering conventions and return the post-operation task object directly with `--json`; `session_id` is null after release. Reuse the existing change indicator and retry conventions rather than introducing agent-specific outcome fields or wrappers.

### Context output

`aven agent context` uses the existing `aven context` arguments, selection, rendering, and JSON structure without checking membership. Add `session_id` to the context's task object. Preserve existing names, types, empty-value conventions, and sections, including dependencies, related tasks, notes, conflicts, epics, recurrence, and attachments.

Do not introduce alternate names such as `blockers`, `dependents`, or `related_tasks`, or place the existing context inside a second wrapper. Keep ordinary task detail and context output consistent with the membership display described below.

### Errors and exit codes

Use the existing argument parser and runtime error path. Success, including an unchanged retry, exits with `0`; argument parsing errors use the parser's existing exit behavior; runtime failures exit with `1`. Do not add a separate domain-error exit code or JSON error format.

Use existing diagnostic conventions to explain failures. Malformed metadata and sync conflicts must be distinguishable from missing membership. Shared operations report the same failures through either command spelling; there is no agent-specific membership or readiness error catalog.

## Metadata implementation

Persist one reserved task metadata value under `aven.agent`, encoded as a compact JSON string:

```json
{ "version": 1, "session_id": "run-42" }
```

This version describes the internal storage format, not public response metadata. Absence means no membership. Keep the storage version and session together so sync treats the record as one metadata value. Release removes that value. Store no duplicate membership column, session table, readiness flag, or agent-specific task status.

The current metadata implementation uses string values and rejects public `aven.*` keys. Add an internal, typed membership writer that can create and mutate this reserved key while preserving existing public restrictions. Reuse metadata field identities, size limits, persistence, sync, conflict, undo, and export/import paths. Audit each path: reserved-key validation must allow legitimate restoration and sync without opening generic user writes.

Generic metadata commands and editors may inspect the value but must not set, remove, or rename the reserved field. The semantic core owns encoding and decoding. Imports and conflict resolution must validate supported records before accepting them as usable membership. Preserve unknown storage versions and report an unsupported storage format rather than rewriting or dropping them.

Malformed or conflicted records are not equivalent to unassigned tasks. Operations interpreting membership use the normal error/conflict presentation; ordinary task operations inherit existing validation rather than adding an agent-only check. Session-filtered queries fail if any task in the workspace's selected deletion scope has uninterpretable agent metadata, because Aven cannot prove whether it belongs to the requested session. This shared query behavior applies equally to `aven list --session` and `aven agent list`. Include affected task references in the diagnostic. Existing conflict tooling remains the explicit repair route.

Expose membership through `session_id`, not by requiring callers to decode reserved metadata. Preserve existing metadata visibility rules on ordinary CLI surfaces.

## Human interface

Show session membership in task detail and context. Add a configurable `agent` table column with header `A`: `·` for valid membership and blank for no membership. Keep the marker independent of task status, including completed tasks. Expose malformed or conflicted metadata through existing error/conflict presentation rather than silently showing an unassigned task.

Include the column in the default layout; allow hiding and reordering through existing table configuration. External metadata and status changes must refresh the TUI without restart. The full session ID belongs in detail, not in the narrow table column.

## Implementation sequence and completion criteria

1. Implement typed session membership in `aven-core`, backed by existing metadata machinery. Complete when assignment, reassignment, release, and no-op behavior pass transactional tests without changing task status.
2. Verify storage lifecycle integration. Complete when restart, sync conflicts, undo, export/import, deletion/restoration, and recurring-occurrence isolation preserve the specified behavior.
3. Implement `aven agent` as thin adapters over membership management and the shared task commands listed above. Complete when every shared command and nested subcommand accepts the same arguments and matches ordinary command behavior, including validation, text/JSON output, retries, undo, sync, and exit codes, with no membership guards. Use equivalence tests against identical initial state for mutations. Cover full agent-prefixed help, global options, membership mutation output, help-only interface version, workspace isolation, and clean JSON stdout.
4. Add shared session filtering and reuse list arguments and execution. Complete when equivalent `aven agent list` and `aven list` requests, both with and without `--session`, produce identical text, JSON, and errors. Test session-specific interactions with availability, deletion scope, recurrence grouping, limits, workspace isolation, and malformed/conflicted metadata. Keep ordinary filter coverage in the shared list tests rather than duplicating a separate agent filter suite.
5. Add TUI membership rendering and refresh coverage. Complete when assignment, release, completion, and external edits are visible without restarting, including custom layouts.
6. Document shipped commands in `CLI.md` and configuration docs; teach `aven prime` and reusable agent guidance the semantic workflow. Complete when examples run against the implementation without raw metadata operations.

## Scope

V1 covers session metadata management and a first-class agent entry point for the shared task workflow defined above. It reuses ordinary command names and semantics rather than creating a separate agent lifecycle. Scheduling, worker identities, leases, heartbeats, distributed claims, automatic session cleanup, session history, and bulk mutation are separate designs. Avoid adding those concepts to the metadata record until their semantics are specified.
