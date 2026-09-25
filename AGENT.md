# Plan: Agent session support

## Goal

Let an external orchestrator use Aven as its task queue and the existing TUI as its UI. Provide a CLI for the agent to mark tasks that are part of its session, the TUI representing those marked tasks with one small dot in the task table. The agent will use the existing task status field for progress.

## Assignment model

- Add an optional `agent-tag` string to each task.
- One assignment per task. Setting another value replaces the assignment.
- Keep the existing statuses: `inbox`, `backlog`, `todo`, `active`, `done`, and `canceled`.
- Status changes do not remove the assignment. Remove it explicitly.

## Proposed CLI

Follow the existing label option pattern.

| Command                                                                 | Purpose                                               |
| ----------------------------------------------------------------------- | ----------------------------------------------------- |
| `aven add <TITLE> --agent-tag <AGENT-TAG>`                              | Create a task assigned to an agent session            |
| `aven edit <TASK_REF> --agent-tag <AGENT-TAG>`                          | Set or replace the assignment                         |
| `aven edit <TASK_REF> --remove-agent-tag <AGENT-TAG>`                   | Remove the assignment if its value matches            |
| `aven list --agent-tag <AGENT-TAG>`                                     | Filter tasks by exact assignment                      |
| `aven bulk-update --all --agent-tag <AGENT-TAG>`                        | Assign all nondeleted tasks                           |
| `aven bulk-update --all --remove-agent-tag <AGENT-TAG>`                 | Remove matching assignments from all nondeleted tasks |
| `aven bulk-update --filter-agent-tag <AGENT-TAG> --set-status <STATUS>` | Change the status of matching tasks                   |

Requirements:

- Support assignment and removal with existing bulk filters, not only `--all`.
- Support the existing bulk `--dry-run` option.
- Reject blank identifiers and conflicting assignment/removal options.
- Combine the list filter with existing filters, including `--status` and `--ready`.
- Include `agent-tag` as a string or `null` in task JSON. Show its value in task detail and context output.
- Keep `aven skill install --agent <AGENT>` unchanged. That existing option selects a skill installation target, not a task session.

Example:

```sh
aven edit APP-7KQ9 --agent-tag session-42
aven list --agent-tag session-42 --ready --json
aven edit APP-7KQ9 --status active
aven edit APP-7KQ9 --status done
aven edit APP-7KQ9 --remove-agent-tag session-42
```

Assignment is not an execution lock. Worker coordination remains the external orchestrator's responsibility.

## TUI agent-tag marker

- Add a narrow `agent-tag` table column with header `A`.
- Show `·` when the task has an assignment. Leave the cell blank otherwise.
- Use the same marker for every task status. Do not add spinners, state icons, or state-dependent colors.
- The dot means “assigned to an agent session.” The existing status column shows progress.
- Keep the marker visible on completed tasks until the assignment is removed.
- Show the full assignment value in task detail so the user can identify the session.
- Include the column in the default table layout. Let users hide or reorder it through `tui.table.columns`.
- Refresh the marker and task status when the external tool changes task data.

## Current icon configuration

Aven does not currently expose settings to replace its task status or priority icons. Their glyphs are defined in `src/tui/widgets.rs`.

The existing `tui.table.compact_status` setting selects icon-only status display. It does not change the glyphs. `tui.table.columns` selects and orders table columns. These settings are defined in `src/config/tui.rs` and documented in `docs/src/content/docs/configuration.md`.

Use a fixed `·` marker for this initial plan. A configurable glyph is a separate, optional change, not an existing capability.

## Implementation steps

1. Add assignment persistence, validation, task reads, and mutations in `aven-core`.
2. Include the field in sync, conflict handling, undo, and data export/import paths.
3. Add the CLI options, filters, JSON field, and detail output.
4. Add the TUI column, layout support, marker, and detail field.
5. Add tests and update `CLI.md` and the relevant configuration documentation.

## Acceptance checks

- Assignment, replacement, matching removal, and nonmatching removal behave as specified.
- List and bulk filters remain workspace-scoped and combine with existing filters.
- Status changes preserve the assignment.
- Assignment survives restart, sync, and export/import. Undo restores the previous value.
- Assigned rows show one dot; unassigned rows show a blank cell.
- External updates appear in the TUI without a restart.
- Custom column layouts and compact status display still work.
- Existing tasks without assignments retain their current behavior.
