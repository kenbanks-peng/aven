use super::creation::{insert_task, record_task_creation_undo, validate_task_draft};
use super::{TaskCreationOptions, TaskCreationUndo, TaskDraft, TaskOutcome};
use crate::db::{Database, begin_immediate};
use crate::ids::TaskId;
use crate::refs::get_task_in_workspace;
use crate::undo::task_snapshot;
use crate::workspaces::Workspace;
use anyhow::{Result, bail};
use sqlx::SqliteConnection;
use std::collections::BTreeMap;
use std::path::Path;
use tracing::{info, warn};

impl Database {
    pub async fn create_task_with_attachments(
        &self,
        workspace: &Workspace,
        blob_dir: &Path,
        lifecycle_policy: crate::attachments::lifecycle::LifecyclePolicy,
        draft: TaskDraft,
        attachments: Vec<crate::operations::TaskAttachmentAddInput>,
    ) -> Result<TaskOutcome> {
        self.create_task_with_attachments_and_undo(
            workspace,
            blob_dir,
            lifecycle_policy,
            draft,
            attachments,
            TaskCreationUndo::None,
        )
        .await
    }

    pub async fn create_task_with_attachments_and_undo(
        &self,
        workspace: &Workspace,
        blob_dir: &Path,
        lifecycle_policy: crate::attachments::lifecycle::LifecyclePolicy,
        draft: TaskDraft,
        attachments: Vec<crate::operations::TaskAttachmentAddInput>,
        undo: TaskCreationUndo,
    ) -> Result<TaskOutcome> {
        self.create_task_with_attachments_and_options(
            workspace,
            blob_dir,
            lifecycle_policy,
            draft,
            attachments,
            TaskCreationOptions::standalone(undo),
        )
        .await
    }

    pub async fn create_task_with_attachments_and_options(
        &self,
        workspace: &Workspace,
        blob_dir: &Path,
        lifecycle_policy: crate::attachments::lifecycle::LifecyclePolicy,
        draft: TaskDraft,
        attachments: Vec<crate::operations::TaskAttachmentAddInput>,
        options: TaskCreationOptions,
    ) -> Result<TaskOutcome> {
        let mut conn = self.acquire_writer().await?;
        create_task_with_attachments_and_epic(
            &mut conn,
            workspace,
            blob_dir,
            lifecycle_policy,
            draft,
            attachments,
            options,
        )
        .await
    }

    pub async fn create_task_with_attachments_for_epic(
        &self,
        workspace: &Workspace,
        blob_dir: &Path,
        lifecycle_policy: crate::attachments::lifecycle::LifecyclePolicy,
        draft: TaskDraft,
        attachments: Vec<crate::operations::TaskAttachmentAddInput>,
        epic_id: &TaskId,
    ) -> Result<TaskOutcome> {
        self.create_task_with_attachments_for_epic_and_undo(
            workspace,
            blob_dir,
            lifecycle_policy,
            draft,
            attachments,
            TaskCreationOptions::for_epic(epic_id.clone(), TaskCreationUndo::None),
        )
        .await
    }

    pub async fn create_task_with_attachments_for_epic_and_undo(
        &self,
        workspace: &Workspace,
        blob_dir: &Path,
        lifecycle_policy: crate::attachments::lifecycle::LifecyclePolicy,
        draft: TaskDraft,
        attachments: Vec<crate::operations::TaskAttachmentAddInput>,
        options: TaskCreationOptions,
    ) -> Result<TaskOutcome> {
        let mut conn = self.acquire_writer().await?;
        create_task_with_attachments_and_epic(
            &mut conn,
            workspace,
            blob_dir,
            lifecycle_policy,
            draft,
            attachments,
            options,
        )
        .await
    }
}

async fn create_task_with_attachments_and_epic(
    conn: &mut SqliteConnection,
    workspace: &Workspace,
    blob_dir: &Path,
    lifecycle_policy: crate::attachments::lifecycle::LifecyclePolicy,
    draft: TaskDraft,
    attachments: Vec<crate::operations::attachments::TaskAttachmentAddInput>,
    options: TaskCreationOptions,
) -> Result<TaskOutcome> {
    validate_task_draft(&draft)?;
    let TaskCreationOptions {
        epic_id,
        undo,
        create_missing_labels,
        require_existing_project,
        capture_undo_snapshot,
        require_existing_epic,
    } = options;
    let mut prepared = Vec::with_capacity(attachments.len());
    for attachment in attachments {
        prepared.push(crate::operations::attachments::prepare_task_attachment(attachment).await?);
    }

    let mut unique = BTreeMap::new();
    for attachment in &prepared {
        unique
            .entry(attachment.sha256.clone())
            .or_insert_with(|| attachment.clone());
    }
    let attachment_hashes = unique.keys().cloned().collect::<Vec<_>>();

    let mut capacity_reservations = Vec::new();
    for attachment in unique.values() {
        let available: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM blob_inventory WHERE sha256 = ? AND available = 1)",
        )
        .bind(&attachment.sha256)
        .fetch_one(&mut *conn)
        .await?;
        if available {
            continue;
        }
        match crate::attachments::lifecycle::ensure_local_capacity(
            conn,
            blob_dir,
            &attachment.sha256,
            attachment.byte_size,
            lifecycle_policy,
            &crate::attachments::lifecycle::SystemClock,
        )
        .await
        {
            Ok(Some(reservation_id)) => capacity_reservations.push(reservation_id),
            Ok(None) => {}
            Err(error) => {
                for reservation_id in capacity_reservations {
                    let _ =
                        crate::attachments::lifecycle::release_reservation(conn, &reservation_id)
                            .await;
                }
                return Err(error);
            }
        }
    }

    let mut staging_leases = Vec::with_capacity(unique.len());
    for attachment in unique.values() {
        match crate::attachments::lifecycle::acquire_lease(
            conn,
            &attachment.sha256,
            "staging",
            &crate::attachments::lifecycle::SystemClock,
        )
        .await
        {
            Ok(lease_id) => staging_leases.push(lease_id),
            Err(error) => {
                for lease_id in staging_leases {
                    let _ = crate::attachments::lifecycle::release_lease(conn, &lease_id).await;
                }
                for reservation_id in capacity_reservations {
                    let _ =
                        crate::attachments::lifecycle::release_reservation(conn, &reservation_id)
                            .await;
                }
                return Err(error);
            }
        }
    }

    let mut created_hashes = Vec::new();
    for attachment in unique.values() {
        match crate::attachments::storage::stage_blob(
            blob_dir,
            &attachment.sha256,
            &attachment.bytes,
        )
        .await
        {
            Ok(staged) if staged.byte_size == attachment.byte_size => {
                if staged.created {
                    created_hashes.push(staged.sha256);
                }
            }
            Ok(_) => {
                cleanup_attachment_guards(conn, &staging_leases, &capacity_reservations).await;
                cleanup_created_objects(conn, blob_dir, &created_hashes).await;
                bail!("error attachment-staged-size-mismatch");
            }
            Err(error) => {
                cleanup_attachment_guards(conn, &staging_leases, &capacity_reservations).await;
                cleanup_created_objects(conn, blob_dir, &created_hashes).await;
                return Err(error);
            }
        }
    }

    let database_result = async {
        let mut tx = begin_immediate(conn).await?;
        if require_existing_epic && let Some(epic_id) = &epic_id {
            crate::operations::epics::require_consumer_epic(&mut tx, workspace, epic_id).await?;
        }
        for attachment in unique.values() {
            crate::attachments::storage::upsert_inventory_available(
                &mut tx,
                &attachment.sha256,
                attachment.byte_size,
                &attachment.facts.media_type,
            )
            .await?;
        }
        let inserted = insert_task(
            &mut tx,
            workspace,
            draft,
            create_missing_labels,
            require_existing_project,
        )
        .await?;
        if let Some(epic_id) = epic_id.as_ref() {
            crate::operations::add_task_to_epic_in_transaction(
                &mut tx,
                workspace,
                &inserted.id,
                epic_id,
            )
            .await?;
        }
        let mut attachment_change_ids = Vec::with_capacity(prepared.len());
        let attachment_base = crate::ids::now_utc();
        for (index, attachment) in prepared.iter().enumerate() {
            let created_at = (attachment_base
                + chrono::TimeDelta::microseconds(i64::try_from(index)?))
            .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
            attachment_change_ids.push(
                crate::operations::attachments::insert_prepared_attachment(
                    &mut tx,
                    workspace,
                    &inserted.id,
                    attachment,
                    &created_at,
                )
                .await?,
            );
        }
        crate::attachments::lifecycle::reconcile_liveness_for_hashes_in_transaction(
            &mut tx,
            &attachment_hashes,
            &crate::attachments::lifecycle::SystemClock,
        )
        .await?;
        let task = get_task_in_workspace(&mut tx, workspace, &inserted.id).await?;
        let attachment_ids = prepared
            .iter()
            .map(|attachment| attachment.attachment_id.clone())
            .collect();
        record_task_creation_undo(
            &mut tx,
            workspace,
            &task,
            &inserted,
            attachment_ids,
            attachment_change_ids.clone(),
            undo,
        )
        .await?;
        let undo_snapshot = if capture_undo_snapshot {
            Some(task_snapshot(&mut tx, &workspace.id, &task.id).await?)
        } else {
            None
        };
        tx.commit().await?;
        Ok::<_, anyhow::Error>((inserted, attachment_change_ids, task, undo_snapshot))
    }
    .await;

    let (inserted, attachment_change_ids, task, undo_snapshot) = match database_result {
        Ok(value) => value,
        Err(error) => {
            cleanup_attachment_guards(conn, &staging_leases, &capacity_reservations).await;
            cleanup_created_objects(conn, blob_dir, &created_hashes).await;
            return Err(error);
        }
    };
    cleanup_attachment_guards(conn, &staging_leases, &capacity_reservations).await;
    info!(
        task_id = %inserted.id,
        project_key = %inserted.project_key,
        label_count = inserted.label_count,
        attachment_count = prepared.len(),
        "task created"
    );
    Ok(TaskOutcome {
        task,
        create_change_id: Some(inserted.change_id),
        attachment_change_ids,
        undo_snapshot,
    })
}

async fn cleanup_attachment_guards(
    conn: &mut SqliteConnection,
    leases: &[String],
    reservations: &[String],
) {
    for lease_id in leases {
        if let Err(error) = crate::attachments::lifecycle::release_lease(conn, lease_id).await {
            warn!(%error, "failed to release attachment staging lease");
        }
    }
    for reservation_id in reservations {
        if let Err(error) =
            crate::attachments::lifecycle::release_reservation(conn, reservation_id).await
        {
            warn!(%error, "failed to release attachment capacity reservation");
        }
    }
}
async fn cleanup_created_objects(conn: &mut SqliteConnection, blob_dir: &Path, hashes: &[String]) {
    for sha256 in hashes {
        crate::attachments::storage::remove_staged_blob_if_unreferenced(conn, blob_dir, sha256)
            .await;
    }
}
