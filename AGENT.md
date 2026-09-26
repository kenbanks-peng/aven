# Agent CLI specification

This is a proposed feature contract, not documentation of commands already implemented. Preserve the architecture in [ARCHITECTURE.md](ARCHITECTURE.md) and the compatibility rules in [SYNC_PROTOCOL.md](SYNC_PROTOCOL.md). Existing command usage is documented in [CLI.md](CLI.md).

## Commands and integration

Group tasks and epics into orchestrator sessions:

| Command                                                        | Behavior                                              |
| -------------------------------------------------------------- | ----------------------------------------------------- |
| `aven agent assign <TASK_REF> --session <SESSION_ID> [--json]` | Set membership, replacing any previous session        |
| `aven agent release <TASK_REF> [--json]`                       | Remove membership                                     |
| `aven agent list --session <SESSION_ID> [OPTIONS]`             | List session members using ordinary task-list options |

Expose these three subcommands. Creation and task workflows use ordinary commands, including `aven add --epic`. Honor existing workspace routing, task-reference resolution, global `--db`/`--workspace` options, help, errors, and exit status. JSON is a command option, not a global flag; this feature does not add `--json` to ordinary `aven edit`.

CLI parsing and orchestration belong under `src/cli/` and `src/commands/`, with dispatch and daemon wake policy integrated through `src/lib.rs`. Domain reads and writes go through owned `aven_core::db::Database` methods. SQLx connections and transaction-local helpers remain private to core; CLI and TUI rendering perform no database access.

## Membership

“Task” includes epics. `SESSION_ID` is an opaque string supplied by the orchestrator, compared exactly without trimming or case normalization. Apply existing metadata value limits, including to empty strings; absence, not an empty string, means unassigned. Session IDs are synced task data, not secrets or authentication credentials.

- Each task belongs to at most one session. Assign and release change only membership and its normal persistence bookkeeping, preserving status, source, and relationships.
- Epic and child membership are independent.
- Membership survives status changes, completion, cancellation, and permitted soft deletion/undeletion. Existing undo and database replacement semantics still apply; membership is not protected from restoring an earlier snapshot.
- Recurring-task membership belongs only to an existing task-backed occurrence. Templates, series references, taskless historical slots, and future occurrences cannot be assigned.
- Membership operations may address existing soft-deleted, paused, archived, or resolved task occurrences through ordinary task-reference resolution. They do not reconcile recurrence, materialize successors, or alter series lifecycle. Do not route them through a generic task mutation that would introduce those side effects.
- Sessions are defined by their members within the selected workspace. There is no session registry or persistent session-selection setting. An unused session ID yields an empty list.

## Writes and unchanged operations

Expose a typed core membership operation backed by the existing serialized writer and an immediate transaction. Within that transaction, resolve the task and canonical reserved field, check membership conflicts, and then compare the requested value with authoritative stored membership.

Assigning the current session or releasing an unassigned task succeeds unchanged. A missing field definition means unassigned; release must not create it. An unchanged operation creates no field, timestamp update, change-log row, field version, or undo entry. Conflict detection takes precedence over unchanged detection, including when the local conflict variant is absence.

Changed writes atomically persist metadata and the existing sync bookkeeping. Follow ordinary CLI mutations' no-undo policy; support the existing optional core/TUI undo mechanism where requested rather than inventing CLI undo history. Trusted undo replay must understand the reserved field. Preserve recurrence undo safety: assigning a successor counts as touching it and must not be ignored by checks that prevent undo from deleting changed successors.

## Listing

Reuse ordinary list parsing, validation, query construction, and rendering in-process. Share Clap conflicts and runtime option checks, not just the final filter struct. Require `--session` for `aven agent list`; ordinary `aven list` remains unscoped.

Apply membership as an AND constraint before recurrence grouping and both SQL-level and post-grouping limits. Preserve ordinary ordering, output formats, `--epics`, option combinations, and recurrence visibility. Do not implement membership using `TaskIdFilter::Only`: that path has explicit-ID ordering and recurrence-visibility semantics different from ordinary lists.

Preserve ordinary availability policy: `--all` changes deletion inclusion, not availability. Keep the existing exceptions for `--deleted`, explicit terminal statuses, and the future-availability selection of `--upcoming`, as well as the nondeleted/nonterminal restrictions of upcoming and overdue lists.

After ordinary recurrence reconciliation, core must own one read transaction covering canonical field lookup, membership-conflict scanning, task selection, and required hydration/grouping. The current pooled-reader list path alone does not guarantee this snapshot. A workspace with no reserved field returns an empty result rather than an unknown-metadata-field error.

Membership is occurrence-specific. Keep terminal-occurrence grouping and its existing series-wide counts; those counts are not session-member counts. A group's representative must come from the membership-filtered results. Preserve the `recurrence_group` schema and series identity; any task-level `session_id` on that row describes its representative, not an assignment of the series. `--expand-recurring` exposes individual task references for assign/release.

## Storage, validation, and compatibility

Persist membership as a task metadata value under the exact reserved key `aven.agent.session_id`, containing the session ID directly (for example, `run-42`). Release removes the value, not the shared field definition. Reuse workspace-local metadata field IDs, alias convergence, value storage, conflicts, and portable data; do not add a parallel session table or a globally fixed field ID.

The current metadata normalizer rejects all `aven.*` keys. Implement distinct canonical lookup/validation, generic authoring permission, and trusted persistence/replay paths; the required reserved-key support does not already exist.

- Generic metadata commands, editors, and core authoring APIs may read but cannot set, remove, or rename this field, or rename another field into it. Leave other reserved keys rejected.
- Validate the resolved canonical field identity, including sync aliases, rather than trusting only the incoming key. Preserve the reserved field's key and meaning across field creation, rename, replay, and conflict resolution.
- Permit this key only on tasks. Reject it on recurrence templates through creation, editing, sync, import, and conflict resolution so template copying cannot assign future occurrences.
- Apply existing per-value, value-count, and aggregate metadata limits. Import and conflict resolution must validate the resulting metadata collection before committing. Invalid resolution leaves values, conflicts, versions, and changes untouched.
- Task undelete preserves stored membership. JSON import, sync apply, and undo use validated reserved-key paths. SQLite backup/restore remains a database snapshot operation, not a replay through the membership writer; preserve its existing safety and compatibility checks.

Existing peers reject this reserved key, even if existing metadata operation names are reused. Treat support as a shared-data contract change under `SYNC_PROTOCOL.md`: cover operation creation gates, wire validation, apply/replay, client/server compatibility, and mixed-version tests. Preserve the frozen baseline; do not silently admit the new key into older contracts. Follow established-protocol gating for offline and local-only databases too. Define and test the permitted JSON-import contract for session-bearing data before shipping, rather than bypassing baseline import validation. A schema migration and a protocol change are separate decisions.

## Conflicts

Assign, release, and session-scoped list fail on unresolved membership conflicts and report affected task references. Use the existing conflict inspection/resolution commands; do not silently pick a winner.

For `aven agent list`, scan all tasks in the selected workspace covered by the requested deletion scope: live tasks by default, deleted tasks with `--deleted`, and both with `--all`. Check before other filters and limits, including tasks hidden by recurrence visibility or assigned locally to a different session. A conflicted local absence must not silently omit a possible member.

Ordinary list, show, context, and unrelated edits retain their existing conflict policy. Merely displaying membership does not make them fail: show the current local value alongside the existing conflict indication, with conflict tools supplying the variants. `session_id` is not proof of resolved membership when the task has a conflict.

## Output and UI

- Add `session_id` to shared task projections and JSON task objects, including compact/list output, full-detail task output, and the separately constructed task in `aven context`. Use `null` for local absence and a string for a local assignment. Preserve ordinary metadata visibility and surrounding output envelopes.
- Assign/release JSON returns the post-operation compact task object directly, not a mutation envelope or series group. Text uses existing mutation rendering and `changed=yes|none` indicators. An empty JSON list is `[]`.
- Show the full, untruncated session ID in task detail and context, safely escaping terminal control characters.
- Batch-load a narrow membership projection for summary reads. Summary hydration deliberately omits metadata values; deriving membership from that empty collection would report false absences. Do not load all metadata or introduce per-row queries merely to render membership.
- Prefix assigned TUI task line items with an AGENT marker using the existing icon/style and display-width conventions, preserving selection, status, and epic markers. Keep it visible in list and columns layouts. On grouped recurrence rows it describes the representative occurrence only.
- Render from the store's published projection, and refresh membership through the existing local-mutation and external/sync refresh paths. Account for marker width in sizing, truncation, and hit testing. Keep existing conflict indicators visible; no new session-management overlay is required.

Implementation entry points: `crates/aven-core/src/metadata/`, `operations/`, `query/`, and `task_enrichment.rs` for domain behavior and projections; `src/task_render/` and `src/commands/context.rs` for CLI output; `src/tui/store/` and `src/tui/ui/task_list/` for TUI refresh and rendering.

## Acceptance coverage

Add focused core, CLI integration, and TUI tests with the implementation:

- Assign, reassign, release, exact/empty IDs, metadata limits, workspace isolation, independent epic/child membership, and unchanged operations in a fresh workspace.
- Atomic persistence and rollback, conflict-before-no-op behavior, generic write/rename rejection, aliases, sync convergence, protocol gating, import validation, and trusted undo replay.
- Membership preservation across status changes and permitted soft deletion/undeletion; occurrence-only writes without reconciliation; rejection on templates; unassigned successors and safe recurrence undo.
- Ordinary/session-list parity across filters, deletion and availability combinations, recurrence visibility/grouping/expansion, ordering, and both limit paths. Cover broad conflict scanning and a consistent membership/task read snapshot.
- Direct mutation JSON, context/detail/summary membership, conflicted ordinary output, empty lists, text change indicators, and TUI marker rendering/refresh without full metadata hydration.

## Example

```sh
aven agent assign APP-7KQ9 --session run-42 --json
aven agent list --session run-42 --ready --expand-recurring --json
aven context APP-7KQ9 --json
aven edit APP-7KQ9 --status active
aven edit APP-7KQ9 --status done
aven agent release APP-7KQ9 --json
```
