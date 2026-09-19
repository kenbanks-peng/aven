use anyhow::{Context, Result};

use crate::tui::app::{App, DetailSection};
use crate::tui::event::{Action, CommandHandler};
use crate::tui::overlay::{CommandState, ConfirmIntent, OverlayState};

impl App {
    pub(super) async fn execute_command_handler(&mut self, handler: CommandHandler) -> Result<()> {
        if let CommandHandler::BuiltIn(action) = handler
            && action != crate::tui::event::Action::BeginCommand
            && matches!(
                self.store.view_state.query,
                crate::tui::store::TaskQuery::Recurring
                    | crate::tui::store::TaskQuery::RecentActions
            )
        {
            return self.execute(action).await;
        }
        let recurrence_series_id = self
            .selected_recurrence_target_id()
            .map(|target| target.series_id);
        let snapshot = self.capture_command_session(recurrence_series_id);
        match handler {
            CommandHandler::BuiltIn(action) => {
                let command = crate::tui::event::COMMANDS
                    .iter()
                    .find(|command| command.action == action)
                    .context("built-in command disappeared from the catalog")?;
                let resolved = match self.resolve_builtin_command(&snapshot, command).await? {
                    Ok(resolved) => resolved,
                    Err(reason) => {
                        self.set_warning(reason);
                        return Ok(());
                    }
                };
                self.execute_resolved_builtin(resolved, &snapshot).await
            }
            CommandHandler::Custom(command_id) => {
                let catalog = self.command_catalog.clone();
                let name = catalog
                    .custom(command_id)
                    .context("custom command disappeared from the catalog")?
                    .name
                    .clone();
                self.execute_captured_custom_command(&catalog, command_id, &name, &snapshot)
                    .await
            }
        }
    }

    pub(super) async fn captured_task_selection(
        &self,
        snapshot: &crate::tui::event::CommandSessionSnapshot,
        action: Action,
    ) -> Result<std::result::Result<Option<crate::tui::task_selection::TaskSelection>, String>>
    {
        use crate::tui::event::CommandTargetPolicy;

        let focused_detail_task = snapshot.detail_focus().and_then(|focus| match focus {
            crate::tui::event::DetailCommandFocus::Relationship { task_id, .. } => Some(task_id),
            _ => None,
        });
        let list_marks = matches!(
            snapshot.surface,
            crate::tui::event::CommandSurfaceSnapshot::List { .. }
        )
        .then(|| snapshot.marked_task_ids())
        .unwrap_or(&[]);
        if let CommandTargetPolicy::Single(label) = action.target_policy()
            && list_marks.len() > 1
        {
            return Ok(Err(format!(
                "{label} requires one task · {} tasks marked",
                list_marks.len()
            )));
        }
        let target_ids = match action.target_policy() {
            CommandTargetPolicy::None
            | CommandTargetPolicy::Attachment
            | CommandTargetPolicy::Recurrence => return Ok(Ok(None)),
            CommandTargetPolicy::Marks => match action {
                Action::ToggleMarkSelected => {
                    snapshot.primary_task_id().into_iter().cloned().collect()
                }
                Action::ToggleMarkAllInView => snapshot.visible_task_ids().to_vec(),
                _ => unreachable!("marks policy action uses an ID-only target"),
            },
            CommandTargetPolicy::Single(_) if !list_marks.is_empty() => list_marks.to_vec(),
            CommandTargetPolicy::Focused
            | CommandTargetPolicy::Single(_)
            | CommandTargetPolicy::Relationship(_) => focused_detail_task
                .or_else(|| snapshot.primary_task_id())
                .into_iter()
                .cloned()
                .collect(),
            CommandTargetPolicy::Batch
                if matches!(
                    snapshot.surface,
                    crate::tui::event::CommandSurfaceSnapshot::Detail { .. }
                ) =>
            {
                focused_detail_task
                    .or_else(|| snapshot.primary_task_id())
                    .into_iter()
                    .cloned()
                    .collect()
            }
            CommandTargetPolicy::Batch if !snapshot.marked_task_ids().is_empty() => {
                snapshot.marked_task_ids().to_vec()
            }
            CommandTargetPolicy::Batch => snapshot.primary_task_id().into_iter().cloned().collect(),
        };
        if target_ids.is_empty() {
            let reason = match action {
                Action::BeginAddNote => "no selected task for note",
                Action::AcceptConflictLocal
                | Action::AcceptConflictRemote
                | Action::BeginManualConflictMerge => "no selected task for conflict resolution",
                _ => "no selected task to edit",
            };
            return Ok(Err(reason.to_string()));
        }
        let anchor_id = snapshot.primary_task_id().unwrap_or(&target_ids[0]).clone();
        let mut hydrate_ids = vec![anchor_id.clone()];
        hydrate_ids.extend(target_ids.iter().filter(|id| **id != anchor_id).cloned());
        let hydrated = self.store.load_task_items(&hydrate_ids).await?;
        if hydrated.len() != hydrate_ids.len() {
            return Ok(Err("a captured task is stale".to_string()));
        }
        let anchor = hydrated
            .iter()
            .find(|item| item.task.id == anchor_id)
            .expect("captured anchor was hydrated");
        let targets = target_ids
            .iter()
            .map(|task_id| {
                hydrated
                    .iter()
                    .find(|item| item.task.id == *task_id)
                    .cloned()
                    .expect("captured target was hydrated")
            })
            .collect();
        let anchor_index = self
            .store
            .tasks
            .iter()
            .position(|item| item.task.id == anchor_id)
            .unwrap_or(0);
        let uses_marks = match action.target_policy() {
            CommandTargetPolicy::Batch | CommandTargetPolicy::Single(_) => !list_marks.is_empty(),
            _ => false,
        };
        Ok(Ok(
            crate::tui::task_selection::TaskSelection::from_captured_with_marks(
                targets,
                anchor,
                anchor_index,
                uses_marks,
            ),
        ))
    }

    pub(super) async fn resolve_builtin_command(
        &self,
        snapshot: &crate::tui::event::CommandSessionSnapshot,
        command: &'static crate::tui::event::BuiltInCommand,
    ) -> Result<std::result::Result<crate::tui::event::ResolvedCommand, String>> {
        use crate::tui::event::{
            CommandSituation, CommandTargetPolicy, DetailCommandFocus, RelationshipTargetPolicy,
            ResolvedCommand, ResolvedCommandTarget,
        };
        if snapshot.workspace.id != self.store.active_workspace.id {
            return Ok(Err("captured workspace is no longer active".to_string()));
        }
        let action = command.action;
        if let crate::tui::event::CommandAvailability::Disabled(reason) =
            crate::tui::event::command_availability(
                crate::tui::event::CatalogCommand::BuiltIn(command),
                snapshot,
                &[],
            )
            && !matches!(reason, crate::tui::event::CommandDisabled::Other(_))
        {
            return Ok(Err(reason.message().to_string()));
        }
        let situation = snapshot.situation();
        let target = if action == Action::ToggleDetail
            && let Some(sidebar) = snapshot.sidebar_target().cloned()
        {
            if let crate::tui::event::SidebarCommandTarget::Project(project) = &sidebar
                && !self
                    .store
                    .projects
                    .iter()
                    .any(|candidate| candidate.key == project.as_str())
            {
                return Ok(Err("captured sidebar project is stale".to_string()));
            }
            ResolvedCommandTarget::Sidebar(sidebar)
        } else if let CommandSituation::SidebarProject { project } = situation
            && matches!(
                action,
                Action::BeginScopeProject
                    | Action::BeginRenameProject
                    | Action::BeginDeleteProject
                    | Action::BeginAddProjectPath
                    | Action::BeginRemoveProjectPath
                    | Action::BeginAddTask
            )
        {
            if !self
                .store
                .projects
                .iter()
                .any(|candidate| candidate.key == project)
            {
                return Ok(Err("captured sidebar project is stale".to_string()));
            }
            ResolvedCommandTarget::SidebarProject(project)
        } else {
            match command.target_policy() {
                CommandTargetPolicy::None => ResolvedCommandTarget::None,
                CommandTargetPolicy::Attachment => {
                    let Some(DetailCommandFocus::Attachment {
                        attachment_id,
                        bytes_present,
                    }) = snapshot.detail_focus()
                    else {
                        return Ok(Err("captured attachment is unavailable".to_string()));
                    };
                    if action == Action::SaveAttachment && !bytes_present {
                        return Ok(Err("attachment bytes are unavailable".to_string()));
                    }
                    let Some(owner) = snapshot.primary_task_id() else {
                        return Ok(Err("captured attachment owner is unavailable".to_string()));
                    };
                    let items = self
                        .store
                        .load_task_items(std::slice::from_ref(owner))
                        .await?;
                    let Some(item) = items.first() else {
                        return Ok(Err("captured attachment owner is stale".to_string()));
                    };
                    let Some(attachment) = item.attachments.iter().find(|attachment| {
                        attachment.attachment_id == *attachment_id && !attachment.deleted
                    }) else {
                        return Ok(Err("captured attachment is stale".to_string()));
                    };
                    if action == Action::SaveAttachment
                        && !self.attachment_bytes_are_available(attachment)
                    {
                        return Ok(Err("attachment bytes are unavailable".to_string()));
                    }
                    ResolvedCommandTarget::Attachment {
                        owner: owner.clone(),
                        attachment_id: attachment_id.clone(),
                    }
                }
                CommandTargetPolicy::Recurrence => {
                    let Some(series_id) = snapshot.recurrence_series_id.clone() else {
                        return Ok(Err("captured recurring series is unavailable".to_string()));
                    };
                    ResolvedCommandTarget::Recurrence(series_id)
                }
                CommandTargetPolicy::Relationship(policy) => {
                    let relationship = match (policy, snapshot.detail_focus()) {
                        (
                            RelationshipTargetPolicy::Dependency,
                            Some(DetailCommandFocus::Relationship {
                                section:
                                    section @ (DetailSection::DependsOn | DetailSection::Blocks),
                                task_id,
                            }),
                        ) => Some((*section, task_id.clone())),
                        (
                            RelationshipTargetPolicy::Related,
                            Some(DetailCommandFocus::Relationship {
                                section: DetailSection::Related,
                                task_id,
                            }),
                        ) => Some((DetailSection::Related, task_id.clone())),
                        (
                            RelationshipTargetPolicy::EpicChild,
                            Some(DetailCommandFocus::Relationship {
                                section: DetailSection::EpicParent,
                                task_id,
                            }),
                        ) => Some((DetailSection::EpicParent, task_id.clone())),
                        (
                            RelationshipTargetPolicy::EpicChild,
                            Some(DetailCommandFocus::Relationship {
                                section: DetailSection::EpicChildren,
                                task_id,
                            }),
                        ) => Some((DetailSection::EpicChildren, task_id.clone())),
                        (_, Some(_)) => {
                            return Ok(Err(
                                "command does not apply to the captured relationship".to_string()
                            ));
                        }
                        (_, None) => None,
                    };
                    let Some((section, related)) = relationship else {
                        let selection = match self.captured_task_selection(snapshot, action).await?
                        {
                            Ok(Some(selection)) => selection,
                            Ok(None) => unreachable!("relationship policy requires a target"),
                            Err(reason) => return Ok(Err(reason)),
                        };
                        if policy == RelationshipTargetPolicy::EpicChild
                            && selection.targets()[0].epic_parent.is_none()
                        {
                            return Ok(Err("captured task is not an epic child".to_string()));
                        }
                        return Ok(Ok(ResolvedCommand {
                            action,
                            target: ResolvedCommandTarget::Tasks(selection),
                            effect: command.surface_effect(),
                        }));
                    };
                    let Some(parent) = snapshot.primary_task_id() else {
                        return Ok(Err("captured parent task is unavailable".to_string()));
                    };
                    ResolvedCommandTarget::Relationship {
                        parent: parent.clone(),
                        related,
                        section,
                    }
                }
                CommandTargetPolicy::Marks if action == Action::ClearMarks => {
                    ResolvedCommandTarget::Marks(snapshot.marked_task_ids().to_vec())
                }
                CommandTargetPolicy::Focused
                | CommandTargetPolicy::Single(_)
                | CommandTargetPolicy::Batch
                | CommandTargetPolicy::Marks => {
                    let selection = match self.captured_task_selection(snapshot, action).await? {
                        Ok(selection) => selection,
                        Err(reason) => return Ok(Err(reason)),
                    };
                    match selection {
                        Some(selection) => ResolvedCommandTarget::Tasks(selection),
                        None => unreachable!("task target policy requires a target"),
                    }
                }
            }
        };
        Ok(Ok(ResolvedCommand {
            action,
            target,
            effect: command.surface_effect(),
        }))
    }

    pub(super) async fn execute_tasks_command(
        &mut self,
        action: Action,
        selection: crate::tui::task_selection::TaskSelection,
    ) -> Result<()> {
        use crate::tui::app::{TaskCopyKind, TaskRefKind};
        match action {
            Action::MoveColumnLeft => self.move_tasks_by_column_for(selection, -1).await?,
            Action::MoveColumnRight => self.move_tasks_by_column_for(selection, 1).await?,
            Action::BeginMoveToColumn => self.open_move_to_column_picker(selection),
            Action::SetStatus(status) => {
                self.submit_edit_status(selection, status.to_string())
                    .await?
            }
            Action::SetPriority(priority) => {
                self.set_exact_priority_for(selection, priority).await?
            }
            Action::CyclePriority(reverse) => self.update_priority_for(selection, reverse).await?,
            Action::CopyShortRef => self.copy_selected_ref_for(&selection, TaskRefKind::Short),
            Action::CopyDurableRef => self.copy_selected_ref_for(&selection, TaskRefKind::Durable),
            Action::CopyTaskTitle => {
                self.copy_selected_task_text_for(&selection, TaskCopyKind::Title)
            }
            Action::CopyTaskDescription => {
                self.copy_selected_task_text_for(&selection, TaskCopyKind::Description)
            }
            Action::CopyTaskText => {
                self.copy_selected_task_text_for(&selection, TaskCopyKind::TitleAndDescription)
            }
            Action::CopyTaskNotes => self.copy_selected_task_notes_for(&selection),
            Action::CopyTaskMarkdown => {
                self.copy_task_markdown_for(&selection.targets()[0].task.id)
                    .await?
            }
            Action::BeginCreateTaskGist => {
                self.begin_create_task_gist_for(selection.targets()[0].task.id.clone())
            }
            Action::BeginEditTitle => self.begin_edit_title_for(selection),
            Action::BeginEditMetadata => self.begin_edit_metadata_for(selection).await?,
            Action::BeginEditDescription => self.begin_edit_description_for(selection),
            Action::BeginEditProject => self.open_edit_project_picker(selection),
            Action::BeginEditPriority => self.begin_edit_priority_for(selection),
            Action::BeginEditEpic => self.open_edit_epic_picker(selection),
            Action::BeginEditAvailability => self.begin_edit_availability_for(selection),
            Action::BeginEditDue => self.begin_edit_due_for(selection),
            Action::BeginEditLabels => self.begin_edit_labels_for(selection),
            Action::Delete => self.begin_delete_task_for(selection),
            Action::Restore => {
                let preserve = self.detail.is_active();
                let result = self
                    .store
                    .mutate_deleted_selection(&selection, false, preserve)
                    .await?;
                self.apply_mutation_result(result);
            }
            Action::BeginStatusPicker => self.begin_status_picker_for(selection),
            Action::BeginAddNote => self.begin_add_note_for(selection),
            Action::BeginAddDependency => self.begin_add_dependency_for(selection).await?,
            Action::BeginRemoveDependency => self.open_remove_dependency_picker(selection),
            Action::BeginAddRelated => self.begin_add_related_for(selection).await?,
            Action::BeginRemoveRelated => self.open_remove_related_picker(selection),
            Action::ToggleMarkSelected => {
                self.list
                    .toggle_mark(selection.targets()[0].task.id.clone());
            }
            Action::ToggleMarkAllInView => {
                let ids = selection.ids().cloned().collect::<Vec<_>>();
                if self.list.all_marked(ids.iter()) {
                    for task_id in &ids {
                        self.list.unmark(task_id);
                    }
                } else {
                    self.list.mark_all(ids);
                }
            }
            Action::ClearMarks => unreachable!("clear marks uses an ID-only target"),
            Action::ShowConflictDetails => {
                self.show_conflict_details_for(&selection.targets()[0])
                    .await?
            }
            Action::AcceptConflictLocal
            | Action::AcceptConflictRemote
            | Action::BeginManualConflictMerge => {
                let targets = self
                    .store
                    .conflict_targets_for(&selection.targets()[0])
                    .await?;
                match action {
                    Action::AcceptConflictLocal => self.begin_conflict_resolution_for(
                        crate::tui::conflict_flow::ConflictResolutionChoice::Local,
                        targets,
                    ),
                    Action::AcceptConflictRemote => self.begin_conflict_resolution_for(
                        crate::tui::conflict_flow::ConflictResolutionChoice::Remote,
                        targets,
                    ),
                    Action::BeginManualConflictMerge => {
                        self.begin_manual_conflict_merge_for(targets)
                    }
                    _ => unreachable!("guarded conflict action"),
                }
            }
            Action::ToggleDetail => {
                let item = selection.targets()[0].clone();
                if let Some(index) = self
                    .store
                    .tasks
                    .iter()
                    .position(|candidate| candidate.task.id == item.task.id)
                {
                    self.list.select_task(Some(index));
                } else {
                    self.store.show_exact_task(item);
                    self.list.select_task(Some(0));
                }
                self.show_detail(0);
            }
            Action::RemoveEpicChild => {
                let item = &selection.targets()[0];
                let Some(target) = self.store.resolve_epic_child_target_for_item(item) else {
                    self.set_warning("Selected task does not belong to an epic");
                    return Ok(());
                };
                let scroll = self.detail.state().map_or(0, |detail| detail.scroll());
                self.overlay = Some(OverlayState::confirm(
                    ConfirmIntent::UnlinkEpicChild {
                        target,
                        restoration: crate::tui::overlay::EpicChildRemovalRestoration {
                            anchor_id: item.task.id.clone(),
                            section: DetailSection::EpicParent,
                            scroll,
                        },
                    },
                    "Unlink relationship",
                    format!(
                        "Unlink {} {} from this task?",
                        item.display_ref, item.task.title
                    ),
                ));
            }
            Action::BeginAddEpicChild => {
                let item = &selection.targets()[0];
                let context = if item.task.is_epic {
                    crate::tui::store::AddEpicChildContext::Existing(
                        crate::tui::store::EpicContext {
                            epic_id: item.task.id.clone(),
                            display_ref: item.display_ref.clone(),
                            project_key: item.task.project_key.clone(),
                        },
                    )
                } else if let Some(parent) = &item.epic_parent {
                    crate::tui::store::AddEpicChildContext::Existing(
                        crate::tui::store::EpicContext {
                            epic_id: parent.task_id.clone(),
                            display_ref: parent.display_ref.clone(),
                            project_key: item.task.project_key.clone(),
                        },
                    )
                } else {
                    crate::tui::store::AddEpicChildContext::Promote(
                        crate::tui::store::EpicContext {
                            epic_id: item.task.id.clone(),
                            display_ref: item.display_ref.clone(),
                            project_key: item.task.project_key.clone(),
                        },
                    )
                };
                match context {
                    crate::tui::store::AddEpicChildContext::Existing(epic) => {
                        self.open_add_epic_child_search(epic)
                    }
                    crate::tui::store::AddEpicChildContext::Promote(epic) => {
                        self.clear_live_search_preview();
                        self.overlay = Some(OverlayState::confirm(
                            ConfirmIntent::PromoteTaskForChild { epic: epic.clone() },
                            "Promote task to epic",
                            format!(
                                "Adding a child will promote {} to an epic. Continue?",
                                epic.display_ref
                            ),
                        ));
                    }
                }
            }
            Action::ToggleEpicExpanded => {
                let index = self
                    .store
                    .tasks
                    .iter()
                    .position(|item| item.task.id == selection.targets()[0].task.id);
                if let Some(result) = self.store.toggle_selected_epic(index).await? {
                    self.list.select_task(result.selected);
                } else {
                    self.set_warning("Select an epic in the Epics list");
                }
            }
            _ => self.set_warning("captured command target is unsupported"),
        }
        Ok(())
    }

    pub(super) async fn execute_resolved_builtin(
        &mut self,
        command: crate::tui::event::ResolvedCommand,
        snapshot: &crate::tui::event::CommandSessionSnapshot,
    ) -> Result<()> {
        use crate::tui::event::{ResolvedCommandTarget, SurfaceEffect};
        if command.effect == SurfaceEffect::ExitDetail {
            self.clear_detail_session();
        }
        match command.target {
            ResolvedCommandTarget::None => self.execute(command.action).await?,
            ResolvedCommandTarget::Marks(task_ids) => {
                for task_id in task_ids {
                    self.list.unmark(&task_id);
                }
            }
            ResolvedCommandTarget::Tasks(selection) => {
                self.execute_tasks_command(command.action, selection)
                    .await?
            }
            ResolvedCommandTarget::Relationship {
                parent,
                related,
                section,
            } => {
                debug_assert_eq!(snapshot.primary_task_id(), Some(&parent));
                let valid_pair = matches!(
                    (command.action, section),
                    (
                        Action::BeginRemoveDependency,
                        DetailSection::DependsOn | DetailSection::Blocks
                    ) | (Action::BeginRemoveRelated, DetailSection::Related)
                        | (
                            Action::RemoveEpicChild,
                            DetailSection::EpicParent | DetailSection::EpicChildren
                        )
                );
                if !valid_pair {
                    self.set_warning("captured relationship command is invalid");
                    return Ok(());
                }
                if let (Some(scroll), Some(detail)) =
                    (snapshot.detail_scroll(), self.detail.state_mut())
                {
                    detail.set_scroll(scroll);
                }
                self.begin_unlink_captured_relationship(snapshot, section, &related)
                    .await?
            }
            ResolvedCommandTarget::Sidebar(sidebar) => {
                if command.action != Action::ToggleDetail {
                    self.set_warning("captured sidebar command is invalid");
                    return Ok(());
                }
                self.apply_sidebar_command_target(sidebar).await?;
            }
            ResolvedCommandTarget::SidebarProject(project) => match command.action {
                Action::BeginScopeProject => {
                    self.show_scope(crate::tui::store::TaskScopeTarget::Project(project))
                        .await?
                }
                Action::BeginRenameProject => self.begin_rename_project_for(Some(&project)),
                Action::BeginDeleteProject => self.begin_delete_project_for(Some(&project)),
                Action::BeginAddProjectPath => self.begin_add_project_path_for(Some(&project)),
                Action::BeginRemoveProjectPath => {
                    self.begin_remove_project_path_for(Some(&project))
                }
                Action::BeginAddTask => self.begin_add_task_for(Some(project)).await?,
                _ => self.execute(command.action).await?,
            },
            ResolvedCommandTarget::Attachment {
                owner,
                attachment_id,
            } => {
                let items = self
                    .store
                    .load_task_items(std::slice::from_ref(&owner))
                    .await?;
                let Some(attachment) = items.first().and_then(|item| {
                    item.attachments.iter().find(|attachment| {
                        attachment.attachment_id == attachment_id && !attachment.deleted
                    })
                }) else {
                    self.set_warning("captured attachment is stale");
                    return Ok(());
                };
                let attachment = attachment.clone();
                let scroll = self.detail.state().map_or(0, |detail| detail.scroll());
                match command.action {
                    Action::OpenAttachment => self.open_attachment_externally(&attachment_id).await,
                    Action::SaveAttachment => {
                        self.begin_save_attachment_metadata(&attachment, scroll)
                    }
                    Action::DeleteAttachment => {
                        self.begin_delete_attachment_metadata(&attachment, scroll)
                    }
                    _ => unreachable!("attachment target policy"),
                }
            }
            ResolvedCommandTarget::Recurrence(series_id) => {
                self.execute_targeted_recurrence_action(
                    Some(crate::tui::overlay::OverlayTarget::RecurrenceSeries {
                        workspace_id: snapshot.workspace.id.clone(),
                        series_id,
                    }),
                    command.action,
                )
                .await?
            }
        }
        Ok(())
    }

    pub(super) async fn accept_command_input(&mut self, state: &CommandState) -> Result<bool> {
        let input = state.input.as_str();
        if input.trim().trim_start_matches(':').is_empty() {
            self.set_info("empty command");
            return Ok(false);
        }
        let candidate = if let Some(highlighted) = state.highlighted {
            state.candidates.get(highlighted)
        } else {
            let normalized = input.trim().trim_start_matches(':');
            let mut ranked = state.candidates.iter().filter_map(|candidate| {
                state.catalog.command(candidate.index).and_then(|command| {
                    crate::tui::event::command_match_rank_for_query(command, normalized)
                        .map(|rank| (rank, candidate))
                })
            });
            let Some((best_rank, first)) = ranked.next() else {
                self.set_warning(format!("unknown command: {}", input.trim()));
                return Ok(false);
            };
            if ranked.any(|(rank, _)| rank == best_rank) {
                self.set_warning(format!("ambiguous command: {}", input.trim()));
                return Ok(false);
            }
            Some(first)
        };
        let Some(candidate) = candidate else {
            self.set_warning(format!("unknown command: {}", input.trim()));
            return Ok(false);
        };
        let Some(command) = state.catalog.command(candidate.index) else {
            self.set_warning("command disappeared from the captured catalog");
            return Ok(true);
        };
        if let Some(reason) = candidate.availability.reason() {
            self.set_warning(format!(":{} is disabled: {reason}", command.name()));
            return Ok(true);
        }
        self.pending_shortcut.clear();
        match command.handler() {
            CommandHandler::Custom(id) => {
                let catalog = state.catalog.clone();
                self.execute_captured_custom_command(
                    &catalog,
                    id,
                    input.trim().trim_start_matches(':'),
                    &state.session,
                )
                .await?;
            }
            CommandHandler::BuiltIn(_) => {
                let built_in = command.built_in().expect("built-in command");
                let resolved = match self
                    .resolve_builtin_command(&state.session, built_in)
                    .await?
                {
                    Ok(resolved) => resolved,
                    Err(reason) => {
                        self.set_warning(format!(":{} is disabled: {reason}", built_in.name));
                        return Ok(true);
                    }
                };
                self.execute_resolved_builtin(resolved, &state.session)
                    .await?;
            }
        }
        Ok(true)
    }

    pub(super) fn move_command_selection(&self, state: &mut CommandState, reverse: bool) {
        if state.candidates.is_empty() {
            state.highlighted = None;
            return;
        }
        state.highlighted = Some(match (state.highlighted, reverse) {
            (Some(0), true) | (None, true) => state.candidates.len() - 1,
            (Some(index), true) => index - 1,
            (Some(index), false) if index + 1 == state.candidates.len() => 0,
            (Some(index), false) => index + 1,
            (None, false) => 0,
        });
    }

    pub(super) fn complete_command_input(&mut self, state: &mut CommandState, reverse: bool) {
        if state.cycle_input.is_none() {
            state.cycle_input = Some(state.input.text.clone());
            state.cycle_candidates = state
                .candidates
                .iter()
                .map(|candidate| candidate.index)
                .collect();
            state.cycle_index = if reverse {
                state.cycle_candidates.len().saturating_sub(1)
            } else {
                0
            };
        } else if !state.cycle_candidates.is_empty() {
            state.cycle_index = if reverse {
                state
                    .cycle_index
                    .checked_sub(1)
                    .unwrap_or(state.cycle_candidates.len() - 1)
            } else {
                (state.cycle_index + 1) % state.cycle_candidates.len()
            };
        }
        let Some(index) = state.cycle_candidates.get(state.cycle_index).copied() else {
            if state
                .input
                .as_str()
                .trim()
                .trim_start_matches(':')
                .is_empty()
            {
                self.set_info("type a command prefix to complete");
            } else {
                self.set_warning(format!(
                    "no command matches: {}",
                    state.input.as_str().trim()
                ));
            }
            state.reset_cycle();
            return;
        };
        let Some(command) = state.catalog.command(index) else {
            state.reset_cycle();
            return;
        };
        state.input.text = command.name().to_string();
        state.input.cursor = state.input.text.len();
        state.refresh_candidates();
        state.highlighted = state
            .candidates
            .iter()
            .position(|candidate| candidate.index == index);
    }
}
