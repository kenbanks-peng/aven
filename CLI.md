# CLI

## Help

| Command | Action |
| --- | --- |
| `aven --help` | Show command help |
| `aven help <COMMAND> [SUBCOMMAND]` | Show detailed command help |
| `aven --version` | Show installed version |
| `aven --db <PATH> <COMMAND>` | Use specified database |
| `aven --workspace <WORKSPACE> <COMMAND>` | Use specified workspace |

## Tasks

| Command | Action |
| --- | --- |
| `aven add <TITLE>` | Create a task |
| `aven add <REQUEST> --natural` | Create task using AI |
| `aven add <TITLE> --epic` | Create an epic |
| `aven list` | List available, nondeleted tasks |
| `aven list --open` | List available unfinished tasks |
| `aven list --ready` | List tasks ready for work |
| `aven list --blocked` | List tasks with incomplete dependencies |
| `aven list --upcoming` | List deferred unfinished tasks |
| `aven list --overdue` | List overdue unfinished tasks |
| `aven list --epics` | List epic containers |
| `aven list --deleted` | List deleted tasks |
| `aven list --all` | Include deleted tasks |
| `aven list --status <STATUS>` | Filter by task status |
| `aven list --priority <PRIORITY>` | Filter by task priority |
| `aven list --project <PROJECT>` | Filter by project |
| `aven list --label <LABEL>` | Filter by label |
| `aven list --metadata <KEY=VALUE>` | Filter by metadata value |
| `aven list --has-metadata <KEY>` | Require metadata field |
| `aven list --missing-metadata <KEY>` | Exclude specified metadata field |
| `aven list --expand-recurring` | Show individual recurring occurrences |
| `aven list --limit <N>` | Limit result count |
| `aven list --json` | Output tasks as JSON |
| `aven search <QUERY>...` | Search workspace tasks |
| `aven context <TASK_REF>` | Show task context |
| `aven show <TASK_REF>` | Show task details |
| `aven show <TASK_REF> --full` | Include complete task details |
| `aven edit <TASK_REF> --title <TITLE>` | Change task title |
| `aven edit <TASK_REF> --description <TEXT>` | Replace task description |
| `aven edit <TASK_REF> --description-file <PATH>` | Read description from file |
| `aven edit <TASK_REF> --description-stdin` | Read description from standard input |
| `aven edit <TASK_REF> --project <PROJECT>` | Move task to project |
| `aven edit <TASK_REF> --status inbox` | Mark task for triage |
| `aven edit <TASK_REF> --status backlog` | Reserve task for later |
| `aven edit <TASK_REF> --status todo` | Mark task ready |
| `aven edit <TASK_REF> --status active` | Mark task in progress |
| `aven edit <TASK_REF> --status done` | Mark task completed |
| `aven edit <TASK_REF> --status canceled` | Mark task canceled |
| `aven edit <TASK_REF> --priority <PRIORITY>` | Change task priority |
| `aven edit <TASK_REF> --available-at <WHEN>` | Defer task availability |
| `aven edit <TASK_REF> --clear-available-at` | Remove availability restriction |
| `aven edit <TASK_REF> --due <WHEN>` | Set task deadline |
| `aven edit <TASK_REF> --clear-due` | Remove task deadline |
| `aven edit <TASK_REF> --epic on` | Convert task to epic |
| `aven edit <TASK_REF> --epic off` | Convert epic to ordinary task |
| `aven edit <TASK_REF> --label <LABEL>` | Add task label |
| `aven edit <TASK_REF> --remove-label <LABEL>` | Remove task label |
| `aven edit <TASK_REF> --metadata <KEY=VALUE>` | Set task metadata |
| `aven edit <TASK_REF> --remove-metadata <KEY>` | Remove task metadata |
| `aven delete <TASK_REF>` | Soft-delete a task |
| `aven restore <TASK_REF>` | Restore a deleted task |

## Notes and descriptions

| Command | Action |
| --- | --- |
| `aven note <TASK_REF> <TEXT>` | Append a task note |
| `aven note <TASK_REF> --file <PATH>` | Append note from file |
| `aven note <TASK_REF> --stdin` | Append note from standard input |
| `aven note-delete <TASK_REF> <NOTE_ID>` | Delete a task note |
| `aven text get <TASK_REF> description` | Read description and hash |
| `aven text get <TASK_REF> description --raw` | Print description bytes only |
| `aven text get <TASK_REF> description --output <PATH>` | Save description; print hash |
| `aven text diff <TASK_REF> description --file <PATH>` | Compare description with file |
| `aven text set <TASK_REF> description --file <PATH> --if-sha256 <HASH>` | Replace description if unchanged |
| `aven text set <TASK_REF> description --stdin --if-sha256 <HASH>` | Replace description from guarded input |

## Bulk updates

| Command | Action |
| --- | --- |
| `aven bulk-update --status <OLD> --set-status <NEW>` | Change matching task statuses |
| `aven bulk-update --project <PROJECT> --set-priority <PRIORITY>` | Change project task priorities |
| `aven bulk-update --filter-label <LABEL> --set-project <PROJECT>` | Move matching labeled tasks |
| `aven bulk-update --all --label <LABEL>` | Label all nondeleted tasks |
| `aven bulk-update --all --remove-label <LABEL>` | Remove label from nondeleted tasks |
| `aven bulk-update --all --metadata <KEY=VALUE>` | Set metadata on nondeleted tasks |
| `aven bulk-update --all --remove-metadata <KEY>` | Remove metadata from nondeleted tasks |
| `aven bulk-update --status <OLD> --set-status <NEW> --dry-run` | Preview changes without writing |

## Relationships

| Command | Action |
| --- | --- |
| `aven dep add <TASK_REF> <BLOCKER_REF>` | Add blocking dependency |
| `aven dep remove <TASK_REF> <BLOCKER_REF>` | Remove blocking dependency |
| `aven dep list <TASK_REF>` | List blockers and dependents |
| `aven related add <TASK_REF> <RELATED_REF>` | Link related tasks |
| `aven related remove <TASK_REF> <RELATED_REF>` | Unlink related tasks |
| `aven related list <TASK_REF>` | List related tasks |
| `aven epic add <CHILD_REF> <EPIC_REF>` | Add child to epic |
| `aven epic remove <CHILD_REF> <EPIC_REF>` | Remove child from epic |
| `aven epic list <EPIC_REF>` | List epic children |

## Recurrence

| Command | Action |
| --- | --- |
| `aven add <TITLE> --repeat <RULE>` | Create recurring task series |
| `aven add <TITLE> --repeat <RULE> --repeat-at <HH:MM>` | Set recurring availability time |
| `aven add <TITLE> --repeat <RULE> --repeat-due none` | Create recurrence without deadlines |
| `aven add <TITLE> --repeat <RULE> --time-zone <IANA_ZONE>` | Set recurrence time zone |
| `aven add <TITLE> --repeat <RULE> --repeat-start-on <YYYY-MM-DD>` | Set recurrence start date |
| `aven recur list` | List recurring series |
| `aven recur show <SERIES_REF>` | Show recurring series |
| `aven recur history <SERIES_REF>` | Show occurrence history |
| `aven recur edit <SERIES_REF> --title <TITLE>` | Rename future occurrences |
| `aven recur edit <SERIES_REF> --status <STATUS>` | Set future occurrence status |
| `aven recur edit <SERIES_REF> --repeat-at <HH:MM>` | Change future availability time |
| `aven recur edit <SERIES_REF> --label <LABEL>` | Replace future occurrence labels |
| `aven recur skip <SERIES_REF>` | Skip current occurrence |
| `aven recur pause <SERIES_REF>` | Pause recurring series |
| `aven recur resume <SERIES_REF>` | Resume paused series |
| `aven recur stop <SERIES_REF>` | Stop future scheduling |
| `aven recur stop <SERIES_REF> --skip-current` | Stop series; skip current occurrence |

## Workspaces and projects

| Command | Action |
| --- | --- |
| `aven workspace list` | List workspaces |
| `aven workspace create <NAME>` | Create workspace |
| `aven workspace rename <WORKSPACE> <NEW_NAME>` | Rename workspace |
| `aven project create <NAME>` | Create project |
| `aven project create <NAME> --path <PATH>` | Create project with directory mapping |
| `aven project list` | List projects |
| `aven project list --search <TEXT>` | Search project names and keys |
| `aven project rename <PROJECT> <NEW_NAME>` | Rename project |
| `aven project delete <PROJECT>` | Delete project |
| `aven project path add <PROJECT> <PATH>` | Map directory to project |
| `aven project path remove <PROJECT> <PATH>` | Remove project directory mapping |
| `aven project path list [PROJECT]` | List project directory mappings |

## Labels and metadata

| Command | Action |
| --- | --- |
| `aven label create <NAME>` | Create label |
| `aven label list` | List labels |
| `aven label list --search <TEXT>` | Search label names |
| `aven label delete <NAME>` | Delete label |
| `aven metadata list` | List metadata fields and usage |
| `aven metadata show <KEY>` | Show metadata field |
| `aven metadata rename <KEY> <NEW_KEY>` | Rename metadata field |

## Attachments

| Command | Action |
| --- | --- |
| `aven attachment add <TASK_REF> <PATH>` | Attach image to task |
| `aven attachment add <TASK_REF> <PATH> --optimize` | Optimize and attach image |
| `aven attachment add <TASK_REF> <PATH> --no-optimize` | Attach original image bytes |
| `aven attachment list <TASK_REF>` | List task attachments |
| `aven attachment list <TASK_REF> --all` | Include deleted attachments |
| `aven attachment get <ATTACHMENT_ID>` | Show attachment metadata |
| `aven attachment get <ATTACHMENT_ID> --output <PATH>` | Save attachment bytes |
| `aven attachment delete <ATTACHMENT_ID>` | Soft-delete attachment |
| `aven attachment prune` | Preview eligible attachment deletions |
| `aven attachment prune --apply` | Delete eligible attachment blobs |

## Sync and conflicts

| Command | Action |
| --- | --- |
| `aven sync` | Synchronize with configured server |
| `aven sync --server <URL>` | Synchronize with specified server |
| `aven sync status` | Report sync health and progress |
| `aven sync pair` | Display iOS pairing invitation |
| `aven sync pair --copy` | Copy credential-bearing pairing invitation |
| `aven server --data <PATH>` | Run local sync server |
| `aven server --data <PATH> --bind <ADDRESS:PORT>` | Run server at specified address |
| `aven conflict list` | List unresolved sync conflicts |
| `aven conflict show <TASK_REF>` | Show task conflict variants |
| `aven conflict diff <TASK_REF> <FIELD>` | Compare conflicting field values |
| `aven conflict export <TASK_REF> <FIELD> --dir <DIR>` | Export conflict variants to files |
| `aven conflict resolve <TASK_REF> <FIELD> --use <VARIANT_TOKEN>` | Resolve using selected variant |
| `aven conflict resolve <TASK_REF> <FIELD> --value <VALUE>` | Resolve using explicit value |
| `aven conflict resolve <TASK_REF> <FIELD> --value-file <PATH>` | Resolve using file contents |
| `aven conflict resolve <TASK_REF> <FIELD> --value-stdin` | Resolve using standard input |

## Daemon

| Command | Action |
| --- | --- |
| `aven daemon` | Run daemon in foreground |
| `aven daemon status` | Report daemon installation and health |
| `aven daemon install` | Install background daemon |
| `aven daemon uninstall` | Uninstall background daemon |
| `aven daemon restart` | Restart background daemon |
| `aven daemon repair` | Repair daemon installation |
| `aven daemon repair --if-installed` | Repair only installed daemon |

## Terminal interface and agents

| Command | Action |
| --- | --- |
| `aven` | Open terminal interface |
| `aven tui` | Open terminal interface |
| `aven tui <TASK_REF>` | Open task details |
| `aven tui --view <VIEW>` | Open specified task view |
| `aven tui --add-task` | Open task composer |
| `aven tui --add-task-only` | Create task, then exit |
| `aven tui --add-task-only --natural` | Compose task using natural language |
| `aven demo` | Explore disposable sample tasks |
| `aven prime` | Print agent guidance and context |
| `aven skill` | Print reusable agent guidance |
| `aven skill install` | Install skill for detected agents |
| `aven skill install --agent <AGENT>` | Install skill for specified agent |

## Configuration and diagnostics

| Command | Action |
| --- | --- |
| `aven config init` | Create local configuration file |
| `aven config show` | Show configuration path and contents |
| `aven config get <KEY>` | Read configuration value |
| `aven config set <KEY> <VALUE>` | Set configuration value |
| `aven doctor` | Diagnose without repairs |
| `aven doctor --integrity` | Check deeper data integrity |
| `aven doctor --fail-on-error` | Fail on reported errors |
| `aven update` | Check for available update |
| `aven update --yes` | Install available direct update |

## Data safety

| Command | Action |
| --- | --- |
| `aven backup` | Back up database and attachments |
| `aven backup --output <PATH>` | Save backup to specified path |
| `aven backup restore <PATH> --yes` | Replace local data from backup |
| `aven export --output <PATH>` | Export JSON without attachment bytes |
| `aven import <PATH> --yes` | Replace local data from JSON |
