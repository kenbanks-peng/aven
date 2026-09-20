use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::ids::{MetadataFieldId, WorkspaceId};

mod fields;
mod validation;
mod values;

pub(crate) const MAX_METADATA_VALUES: usize = 128;
pub(crate) const MAX_METADATA_VALUE_BYTES: usize = 4 * 1024;
pub(crate) const MAX_METADATA_TOTAL_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataField {
    pub id: MetadataFieldId,
    pub workspace_id: WorkspaceId,
    pub key: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskMetadataValue {
    pub field_id: MetadataFieldId,
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskMetadataInput {
    /// Require this workspace field identity and key instead of defining a key.
    pub expected_field_id: Option<MetadataFieldId>,
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataFieldUsage {
    pub field: MetadataField,
    pub task_count: usize,
    pub series_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ResolvedMetadataValue {
    pub(crate) field_id: MetadataFieldId,
    pub(crate) key: String,
    pub(crate) value: String,
}

#[derive(Serialize, Deserialize)]
struct MetadataConflictValue {
    present: bool,
    value: String,
}

pub(crate) fn encode_metadata_conflict_value(value: Option<&str>) -> Result<String> {
    Ok(serde_json::to_string(&MetadataConflictValue {
        present: value.is_some(),
        value: value.unwrap_or_default().to_string(),
    })?)
}

pub(crate) fn decode_metadata_conflict_value(value: &str) -> Result<Option<String>> {
    let value: MetadataConflictValue = serde_json::from_str(value)?;
    Ok(value.present.then_some(value.value))
}

pub use fields::normalize_metadata_key;

pub(crate) use fields::{
    find_metadata_field_in_workspace, metadata_field_by_id, metadata_field_by_key,
    require_metadata_field,
};
pub(crate) use validation::{
    validate_metadata_update, validate_recurrence_metadata_result, validate_task_metadata_result,
};
pub(crate) use values::{
    insert_initial_task_metadata, metadata_by_task_ids, remove_recurrence_metadata,
    remove_task_metadata, resolve_metadata_inputs, set_recurrence_metadata, set_task_metadata,
};

#[cfg(test)]
pub(crate) use fields::{rename_metadata_field, resolve_or_create_metadata_field};
#[cfg(test)]
use validation::validate_metadata_result_limits;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::begin_immediate;
    use crate::test_support::{ensure_default_workspace, test_conn};

    #[tokio::test]
    async fn existing_field_identity_rejects_rename_and_other_workspace() {
        let (_temp, mut conn) = test_conn().await;
        let workspace = ensure_default_workspace(&mut conn).await.unwrap();
        let mut tx = begin_immediate(&mut conn).await.unwrap();
        let field = resolve_or_create_metadata_field(&mut tx, &workspace, "review")
            .await
            .unwrap();
        let input = TaskMetadataInput {
            expected_field_id: Some(field.id.clone()),
            key: field.key.clone(),
            value: String::new(),
        };
        assert_eq!(
            resolve_metadata_inputs(&mut tx, &workspace, std::slice::from_ref(&input))
                .await
                .unwrap()[0]
                .value,
            ""
        );
        rename_metadata_field(&mut tx, &workspace, "review", "review-state")
            .await
            .unwrap();
        assert!(
            resolve_metadata_inputs(&mut tx, &workspace, &[input])
                .await
                .unwrap_err()
                .to_string()
                .contains("metadata-field-changed")
        );
        assert!(
            metadata_field_by_key(&mut tx, &workspace.id, "review")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            require_metadata_field(&mut tx, &WorkspaceId::new(), &field.id, "review-state")
                .await
                .is_err()
        );
    }

    #[test]
    fn metadata_keys_have_stable_normalization() {
        assert_eq!(normalize_metadata_key(" Max_Turns ").unwrap(), "max_turns");
        assert_eq!(normalize_metadata_key("source.id").unwrap(), "source.id");
        assert!(normalize_metadata_key("1source").is_err());
        assert!(normalize_metadata_key("source id").is_err());
        assert!(normalize_metadata_key("aven.internal").is_err());
    }

    #[test]
    fn metadata_result_limits_include_existing_value_bytes() {
        let mut existing = (0..7)
            .map(|index| (format!("key_{index}"), "x".repeat(MAX_METADATA_VALUE_BYTES)))
            .collect::<Vec<_>>();
        existing.push((
            "key_7".to_string(),
            "x".repeat(MAX_METADATA_VALUE_BYTES - 1),
        ));
        let set = [TaskMetadataInput {
            expected_field_id: None,
            key: "key_8".to_string(),
            value: "é".to_string(),
        }];

        let error = validate_metadata_result_limits(existing, &set, &[]).unwrap_err();

        assert_eq!(
            error.to_string(),
            format!("error metadata-values-too-large limit={MAX_METADATA_TOTAL_BYTES}")
        );
    }

    #[test]
    fn metadata_result_limits_apply_removals_before_sets() {
        let existing = (0..8)
            .map(|index| (format!("key_{index}"), "x".repeat(MAX_METADATA_VALUE_BYTES)))
            .collect::<Vec<_>>();
        let set = [TaskMetadataInput {
            expected_field_id: None,
            key: "replacement".to_string(),
            value: "y".repeat(MAX_METADATA_VALUE_BYTES),
        }];

        validate_metadata_result_limits(existing, &set, &[" KEY_0 ".to_string()]).unwrap();
    }

    #[test]
    fn metadata_result_limits_distinguish_replacement_from_insertion_at_count_limit() {
        let existing = (0..MAX_METADATA_VALUES)
            .map(|index| {
                (
                    format!("key_{index}"),
                    "x".repeat(MAX_METADATA_TOTAL_BYTES / MAX_METADATA_VALUES),
                )
            })
            .collect::<Vec<_>>();
        let replacement = [TaskMetadataInput {
            expected_field_id: None,
            key: "KEY_0".to_string(),
            value: "replacement".to_string(),
        }];
        validate_metadata_result_limits(existing.clone(), &replacement, &[]).unwrap();

        let insertion = [TaskMetadataInput {
            expected_field_id: None,
            key: "extra".to_string(),
            value: "x".to_string(),
        }];
        let error = validate_metadata_result_limits(existing, &insertion, &[]).unwrap_err();

        assert_eq!(
            error.to_string(),
            format!("error too-many-metadata-values limit={MAX_METADATA_VALUES}")
        );
    }

    #[tokio::test]
    async fn repeated_key_resolution_reuses_stable_field_identity() {
        let (_temp, mut conn) = test_conn().await;
        let workspace = ensure_default_workspace(&mut conn).await.unwrap();
        let mut tx = begin_immediate(&mut conn).await.unwrap();
        let first = resolve_or_create_metadata_field(&mut tx, &workspace, "max_turns")
            .await
            .unwrap();
        let second = resolve_or_create_metadata_field(&mut tx, &workspace, "MAX_TURNS")
            .await
            .unwrap();
        tx.commit().await.unwrap();

        assert_eq!(first.id, second.id);
        let changes: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM changes WHERE op_type = 'create_metadata_field'",
        )
        .fetch_one(&mut *conn)
        .await
        .unwrap();
        assert_eq!(changes, 1);
    }
}
