use anyhow::Result;

use crate::tui::app::{App, DetailSection, DetailTargetId};
use crate::tui::overlay::{ConfirmIntent, OverlayState};

#[derive(Clone)]
pub(super) struct FocusedRelationship {
    pub(super) section: DetailSection,
    pub(super) task_id: crate::ids::TaskId,
    pub(super) display_ref: String,
    pub(super) title: String,
}

impl App {
    pub(super) async fn remove_selected_epic_child(&mut self) -> Result<()> {
        let detail = self.detail.is_active();
        let focused_child = match self
            .detail
            .state()
            .and_then(|detail| detail.focused_target())
        {
            Some(DetailTargetId::Task {
                section: DetailSection::EpicChildren,
                task_id,
            }) => Some(task_id),
            _ => None,
        };
        let focused_child = detail.then_some(focused_child).flatten();
        if let (Some(removed), Some(child_id)) = (
            self.detail
                .state()
                .and_then(|detail| detail.removed_epic_child()),
            focused_child,
        ) && removed.child.task_id == *child_id
        {
            self.set_info(format!(
                "{} is already removed from its epic",
                removed.child.display_ref
            ));
            return Ok(());
        }
        let Some(target) = self
            .store
            .resolve_epic_child_target(self.list.selected_task(), focused_child)
        else {
            if detail
                && self
                    .store
                    .selected_task(self.list.selected_task())
                    .is_some_and(|item| item.task.is_epic)
            {
                self.set_warning("Select a child with Tab first");
            } else {
                self.set_warning("Selected task does not belong to an epic");
            }
            return Ok(());
        };
        let mutation = self.store.remove_epic_child(target).await?;
        self.list.select_task(mutation.message.selected);
        if detail
            && mutation.changed
            && let Some(detail) = self.detail.state_mut()
        {
            detail.set_removed_epic_child(Some(crate::tui::app::RemovedEpicChild {
                epic_id: mutation.epic.epic_id,
                child: mutation.child.clone(),
                original_position: mutation.original_position,
            }));
            detail.set_focused_target(Some(DetailTargetId::Task {
                section: DetailSection::EpicChildren,
                task_id: mutation.child.task_id,
            }));
        }
        self.set_mutation_success(mutation.message.message);
        Ok(())
    }

    pub(super) async fn submit_unlink_epic_child(
        &mut self,
        target: crate::tui::store::EpicChildTarget,
        restoration: crate::tui::overlay::EpicChildRemovalRestoration,
    ) -> Result<()> {
        let mut mutation = self.store.remove_epic_child(target).await?;
        if restoration.section == DetailSection::EpicParent {
            mutation.message.selected = self.store.refresh(Some(&restoration.anchor_id)).await?;
        }
        self.list.select_task(mutation.message.selected);
        if mutation.changed
            && restoration.section == DetailSection::EpicChildren
            && let Some(detail) = self.detail.state_mut()
        {
            detail.set_scroll(restoration.scroll);
            detail.set_removed_epic_child(Some(crate::tui::app::RemovedEpicChild {
                epic_id: mutation.epic.epic_id,
                child: mutation.child.clone(),
                original_position: mutation.original_position,
            }));
            detail.set_focused_target(Some(DetailTargetId::Task {
                section: DetailSection::EpicChildren,
                task_id: mutation.child.task_id,
            }));
        }
        self.set_mutation_success(mutation.message.message);
        Ok(())
    }

    pub(super) fn focused_relationship(&self) -> Option<FocusedRelationship> {
        let DetailTargetId::Task { section, task_id } = self
            .detail
            .state()
            .and_then(|detail| detail.focused_target())?
        else {
            return None;
        };
        let item = self.store.selected_task(self.list.selected_task())?;
        if *section == DetailSection::Related {
            let link = item
                .related
                .iter()
                .find(|link| (!link.deleted || item.task.deleted) && link.task_id == *task_id)?;
            return Some(FocusedRelationship {
                section: *section,
                task_id: link.task_id.clone(),
                display_ref: link.display_ref.clone(),
                title: link.title.clone(),
            });
        }
        let link = match section {
            DetailSection::EpicParent => item
                .epic_parent
                .as_ref()
                .filter(|link| link.task_id == *task_id),
            DetailSection::EpicChildren => item
                .epic_children
                .iter()
                .find(|link| link.task_id == *task_id)
                .or_else(|| {
                    self.detail
                        .state()
                        .and_then(|detail| detail.removed_epic_child())
                        .map(|removed| &removed.child)
                        .filter(|link| link.task_id == *task_id)
                }),
            DetailSection::DependsOn => {
                item.depends_on.iter().find(|link| link.task_id == *task_id)
            }
            DetailSection::Blocks => item.blocks.iter().find(|link| link.task_id == *task_id),
            DetailSection::Related
            | DetailSection::Attachments
            | DetailSection::Notes
            | DetailSection::CustomMetadata
            | DetailSection::Activity => None,
        }?;
        Some(FocusedRelationship {
            section: *section,
            task_id: link.task_id.clone(),
            display_ref: link.display_ref.clone(),
            title: link.title.clone(),
        })
    }

    pub(super) async fn focused_relationship_selection(
        &self,
        relationship: &FocusedRelationship,
    ) -> Result<Option<crate::tui::task_selection::TaskSelection>> {
        let Some(anchor_index) = self.list.selected_task() else {
            return Ok(None);
        };
        let Some(anchor) = self.store.selected_task(Some(anchor_index)) else {
            return Ok(None);
        };
        let Some(target) = self.store.load_task_item(&relationship.task_id).await? else {
            return Ok(None);
        };
        Ok(Some(
            crate::tui::task_selection::TaskSelection::for_detail_target(
                target,
                anchor,
                anchor_index,
            ),
        ))
    }

    pub(super) async fn begin_unlink_focused_relationship(
        &mut self,
        relationship: &FocusedRelationship,
    ) -> Result<()> {
        let anchor_id = self
            .store
            .selected_task(self.list.selected_task())
            .map(|item| item.task.id.clone());
        let scroll = self.detail.state().map_or(0, |detail| detail.scroll());
        let intent = match relationship.section {
            DetailSection::DependsOn => {
                let Some(selection) = self.resolve_task_selection() else {
                    self.set_info("no selected task to edit");
                    return Ok(());
                };
                ConfirmIntent::UnlinkDependency {
                    selection,
                    depends_on_task_id: relationship.task_id.clone(),
                }
            }
            DetailSection::Blocks => {
                let Some(selection) = self.focused_relationship_selection(relationship).await?
                else {
                    self.set_warning("linked task is unavailable");
                    return Ok(());
                };
                let Some(depends_on_task_id) = self
                    .store
                    .selected_task(self.list.selected_task())
                    .map(|item| item.task.id.clone())
                else {
                    self.set_info("no selected task to edit");
                    return Ok(());
                };
                ConfirmIntent::UnlinkDependency {
                    selection,
                    depends_on_task_id,
                }
            }
            DetailSection::Related => {
                let Some(selection) = self.resolve_task_selection() else {
                    self.set_info("no selected task to edit");
                    return Ok(());
                };
                ConfirmIntent::UnlinkRelated {
                    selection,
                    related_task_id: relationship.task_id.clone(),
                }
            }
            DetailSection::EpicParent => {
                let Some(target) = self
                    .store
                    .resolve_epic_child_target(self.list.selected_task(), None)
                else {
                    self.set_warning("focused epic relationship is unavailable");
                    return Ok(());
                };
                ConfirmIntent::UnlinkEpicChild {
                    target,
                    restoration: crate::tui::overlay::EpicChildRemovalRestoration {
                        anchor_id: anchor_id
                            .clone()
                            .expect("focused relationship has a selected task"),
                        section: relationship.section,
                        scroll,
                    },
                }
            }
            DetailSection::EpicChildren => {
                let Some(target) = self.store.resolve_epic_child_target(
                    self.list.selected_task(),
                    Some(&relationship.task_id),
                ) else {
                    self.set_warning("focused epic relationship is unavailable");
                    return Ok(());
                };
                ConfirmIntent::UnlinkEpicChild {
                    target,
                    restoration: crate::tui::overlay::EpicChildRemovalRestoration {
                        anchor_id: anchor_id
                            .clone()
                            .expect("focused relationship has a selected task"),
                        section: relationship.section,
                        scroll,
                    },
                }
            }
            DetailSection::Attachments
            | DetailSection::Notes
            | DetailSection::CustomMetadata
            | DetailSection::Activity => {
                self.set_warning("focused row does not support unlink");
                return Ok(());
            }
        };
        self.overlay = Some(OverlayState::confirm(
            intent,
            "Unlink relationship",
            format!(
                "Unlink {} {} from this task?",
                relationship.display_ref, relationship.title
            ),
        ));
        Ok(())
    }

    pub(super) async fn begin_unlink_captured_relationship(
        &mut self,
        snapshot: &crate::tui::event::CommandSessionSnapshot,
        section: DetailSection,
        related_task_id: &crate::ids::TaskId,
    ) -> Result<()> {
        let Some(parent_id) = snapshot.primary_task_id() else {
            self.set_warning("captured parent task is unavailable");
            return Ok(());
        };
        let items = self
            .store
            .load_task_items(&[parent_id.clone(), related_task_id.clone()])
            .await?;
        let Some(parent) = items.iter().find(|item| item.task.id == *parent_id) else {
            self.set_warning("captured parent task is stale");
            return Ok(());
        };
        if section == DetailSection::Related {
            let Some(link) = parent.related.iter().find(|link| {
                (!link.deleted || parent.task.deleted) && link.task_id == *related_task_id
            }) else {
                self.set_warning("captured relationship is stale");
                return Ok(());
            };
            let selection = crate::tui::task_selection::TaskSelection::from_captured(
                vec![parent.clone()],
                parent,
                0,
            )
            .expect("captured parent selection is non-empty");
            self.overlay = Some(OverlayState::confirm(
                ConfirmIntent::UnlinkRelated {
                    selection,
                    related_task_id: related_task_id.clone(),
                },
                "Unlink relationship",
                format!("Unlink {} {} from this task?", link.display_ref, link.title),
            ));
            return Ok(());
        }
        let link = match section {
            DetailSection::EpicParent => parent
                .epic_parent
                .as_ref()
                .filter(|link| link.task_id == *related_task_id),
            DetailSection::EpicChildren => parent
                .epic_children
                .iter()
                .find(|link| link.task_id == *related_task_id),
            DetailSection::DependsOn => parent
                .depends_on
                .iter()
                .find(|link| link.task_id == *related_task_id),
            DetailSection::Blocks => parent
                .blocks
                .iter()
                .find(|link| link.task_id == *related_task_id),
            DetailSection::Related
            | DetailSection::Attachments
            | DetailSection::Notes
            | DetailSection::CustomMetadata
            | DetailSection::Activity => None,
        };
        let Some(link) = link else {
            self.set_warning("captured relationship is stale");
            return Ok(());
        };
        let intent = match section {
            DetailSection::DependsOn => {
                let selection = crate::tui::task_selection::TaskSelection::from_captured(
                    vec![parent.clone()],
                    parent,
                    0,
                )
                .expect("captured parent selection is non-empty");
                ConfirmIntent::UnlinkDependency {
                    selection,
                    depends_on_task_id: related_task_id.clone(),
                }
            }
            DetailSection::Blocks => {
                let Some(blocked) = items.iter().find(|item| item.task.id == *related_task_id)
                else {
                    self.set_warning("captured linked task is stale");
                    return Ok(());
                };
                let selection = crate::tui::task_selection::TaskSelection::from_captured(
                    vec![blocked.clone()],
                    parent,
                    0,
                )
                .expect("captured linked selection is non-empty");
                ConfirmIntent::UnlinkDependency {
                    selection,
                    depends_on_task_id: parent_id.clone(),
                }
            }
            DetailSection::EpicParent => {
                let Some(epic) = items.iter().find(|item| item.task.id == *related_task_id) else {
                    self.set_warning("captured epic task is stale");
                    return Ok(());
                };
                let Some((original_position, child)) = epic
                    .epic_children
                    .iter()
                    .enumerate()
                    .find(|(_, child)| child.task_id == *parent_id)
                else {
                    self.set_warning("captured epic relationship is stale");
                    return Ok(());
                };
                ConfirmIntent::UnlinkEpicChild {
                    target: crate::tui::store::EpicChildTarget {
                        epic: crate::tui::store::EpicContext {
                            epic_id: epic.task.id.clone(),
                            display_ref: epic.display_ref.clone(),
                            project_key: epic.task.project_key.clone(),
                        },
                        child: child.clone(),
                        original_position,
                    },
                    restoration: crate::tui::overlay::EpicChildRemovalRestoration {
                        anchor_id: parent_id.clone(),
                        section,
                        scroll: snapshot.detail_scroll().unwrap_or(0),
                    },
                }
            }
            DetailSection::EpicChildren => {
                let original_position = parent
                    .epic_children
                    .iter()
                    .position(|child| child.task_id == *related_task_id)
                    .expect("captured child link has a position");
                ConfirmIntent::UnlinkEpicChild {
                    target: crate::tui::store::EpicChildTarget {
                        epic: crate::tui::store::EpicContext {
                            epic_id: parent.task.id.clone(),
                            display_ref: parent.display_ref.clone(),
                            project_key: parent.task.project_key.clone(),
                        },
                        child: link.clone(),
                        original_position,
                    },
                    restoration: crate::tui::overlay::EpicChildRemovalRestoration {
                        anchor_id: parent_id.clone(),
                        section,
                        scroll: snapshot.detail_scroll().unwrap_or(0),
                    },
                }
            }
            DetailSection::Related
            | DetailSection::Attachments
            | DetailSection::Notes
            | DetailSection::CustomMetadata
            | DetailSection::Activity => {
                self.set_warning("captured row does not support unlink");
                return Ok(());
            }
        };
        self.overlay = Some(OverlayState::confirm(
            intent,
            "Unlink relationship",
            format!("Unlink {} {} from this task?", link.display_ref, link.title),
        ));
        Ok(())
    }
}
