use super::super::hit_test::task_list_status_area;
use super::super::preview::task_preview_fields_line;
use super::super::view_model::TaskListProjection;
use super::*;
use crate::tui::widgets::title_cell;

#[tokio::test]
async fn task_status_at_position_only_hits_status_column() {
    let store = test_store_with_tasks(vec![task_list_item("task")]).await;
    let table_state = TableState::default();
    let area = Rect::new(0, 0, 140, 10);
    let task_id = store.tasks[0].task.id.clone();

    let projection = TaskListProjection::from_table_state(
        &store,
        &table_state,
        area.height.saturating_sub(1) as usize,
    );
    let status_area = task_list_status_area(&store, &projection, area, 1);
    let hit = task_status_at_position(&store, &table_state, area, status_area.x, 2).unwrap();
    assert_eq!(hit.task_index, 0);
    assert_eq!(hit.task_id, task_id);

    assert!(task_status_at_position(&store, &table_state, area, status_area.x - 1, 2).is_none());
    assert!(
        task_status_at_position(
            &store,
            &table_state,
            area,
            status_area.x.saturating_add(status_area.width),
            2
        )
        .is_none()
    );
}

#[tokio::test]
async fn epic_status_hit_testing_tracks_parent_and_expanded_child_rows() {
    for width in [64, 120] {
        let collapsed = epic_test_store(false).await;
        let table_state = TableState::default();
        let area = Rect::new(0, 0, width, 5);
        let projection = TaskListProjection::from_table_state(
            &collapsed,
            &table_state,
            area.height.saturating_sub(1) as usize,
        );
        let status_area = task_list_status_area(&collapsed, &projection, area, 0);
        let parent_hit =
            task_status_at_position(&collapsed, &table_state, area, status_area.x, area.y + 1)
                .unwrap();
        assert_eq!(parent_hit.task_id, collapsed.tasks[0].task.id);

        let expanded = epic_test_store(true).await;
        let projection = TaskListProjection::from_table_state(
            &expanded,
            &table_state,
            area.height.saturating_sub(1) as usize,
        );
        let status_area = task_list_status_area(&expanded, &projection, area, 1);
        let child_hit =
            task_status_at_position(&expanded, &table_state, area, status_area.x, area.y + 2)
                .unwrap();
        assert_eq!(child_hit.task_id, expanded.tasks[1].task.id);
    }
}

#[tokio::test]
async fn task_status_at_position_respects_wide_sidebar_offset() {
    let store = test_store_with_tasks(vec![task_list_item("task")]).await;
    let table_state = TableState::default();
    let area = Rect::new(26, 2, 114, 18);
    let task_id = store.tasks[0].task.id.clone();

    let projection = TaskListProjection::from_table_state(
        &store,
        &table_state,
        area.height.saturating_sub(1) as usize,
    );
    let status_area = task_list_status_area(&store, &projection, area, 1);
    let hit = task_status_at_position(&store, &table_state, area, status_area.x, 4).unwrap();

    assert_eq!(hit.task_index, 0);
    assert_eq!(hit.task_id, task_id);
}

#[tokio::test]
async fn compact_status_column_shows_single_letter_header_and_icon() {
    let mut store = test_store_with_tasks(vec![task_list_item("task")]).await;
    let mut config = crate::config::AppConfig::default();
    config.tui.table.compact_status = true;
    store.set_config(config);

    let area = Rect::new(0, 0, 140, 8);
    let buffer = render_task_list_buffer(&store, area.width, area.height);
    let table_state = TableState::default();
    let projection = TaskListProjection::from_table_state(
        &store,
        &table_state,
        area.height.saturating_sub(1) as usize,
    );
    let visual_row = task_visual_row(&store, 0).unwrap();
    let status_area = task_list_status_area(&store, &projection, area, visual_row as u16);

    assert_eq!(status_area.width, 1);
    assert_eq!(buffer[(status_area.x, area.y)].symbol(), "S");
    assert_eq!(buffer[(status_area.x, status_area.y)].symbol(), "□");
    let rendered = buffer_text(&buffer);
    assert!(!rendered.contains("STATUS"), "{rendered}");
    assert!(!rendered.contains("□ todo"), "{rendered}");

    let hit =
        task_status_at_position(&store, &table_state, area, status_area.x, status_area.y).unwrap();
    assert_eq!(hit.task_index, 0);
    assert!(
        task_status_at_position(
            &store,
            &table_state,
            area,
            status_area.x.saturating_add(1),
            status_area.y,
        )
        .is_none()
    );
}

#[test]
fn deleted_row_marks_metadata_column_and_keeps_status() {
    let mut item = task_list_item("original title");
    item.task.deleted = true;

    let buffer = render_task_row_buffer(&item, None);
    let rendered = buffer_text(&buffer);
    let cells = build_task_row_cells(
        &item,
        TaskTimeContext {
            now_seconds: 0,
            render_mode: TaskListRenderMode::Flat,
            due_order: false,
            show_due: true,
        },
        None,
        &[12, 40, 12, 6, 9, 10, 3, 5, 6],
        TaskRowState {
            selected: false,
            focused: false,
            marked: false,
        },
        EpicSelectionContext::default(),
    );

    assert!(rendered.contains("original title"));
    assert!(!rendered.contains("deleted original title"));
    assert_eq!(cells[3].to_string(), "×");
    assert_eq!(cells[5].to_string(), "□ todo");
    assert!(
        task_preview_fields_line(&item)
            .to_string()
            .contains("deleted yes")
    );
}

#[test]
fn recurring_rows_and_preview_show_series_context() {
    let mut item = task_list_item("daily review");
    let series_id: aven_core::recurrence::RecurrenceSeriesId = "7KQ9A1X4MV2P8D6R".parse().unwrap();
    item.recurrence = Some(crate::query::TaskRecurrenceSummary {
        series_id: series_id.clone(),
        series_ref: "RCR-A1".to_string(),
        slot_on: "2026-07-20".to_string(),
        rule_label: "daily at 09:00".to_string(),
        timezone: "Europe/Helsinki".to_string(),
        lifecycle: aven_core::recurrence::RecurrenceSeriesState::Active,
        outcome: None,
        projection_state: aven_core::recurrence::RecurrenceProjectionState::Projected,
    });
    item.recurrence_group = Some(crate::query::RecurrenceTaskGroup {
        series_id,
        series_ref: "RCR-A1".to_string(),
        counts: crate::query::RecurrenceCounts {
            series_ref: "RCR-A1".to_string(),
            completed: 4,
            skipped: 2,
            missed: 1,
            latest_slot_on: Some("2026-07-20".to_string()),
            ..crate::query::RecurrenceCounts::default()
        },
    });

    let row_text = title_cell(&item, 80).to_string();
    assert!(row_text.contains("↻"));
    assert!(row_text.contains("2026-07-20"));
    assert!(row_text.contains("RCR-A1"));
    assert!(row_text.contains("✓4"));
    assert!(row_text.contains("↷2"));
    assert!(row_text.contains("×1"));

    let preview = super::super::preview::task_preview_lines(&item, 80, 20)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(preview.contains("RCR-A1"));
    assert!(preview.contains("slot 2026-07-20"));
    assert!(preview.contains("daily at 09:00"));
    assert!(preview.contains("active"));
    assert!(preview.contains("4 completed"));
    assert!(preview.contains("2 skipped"));
    assert!(preview.contains("1 missed"));
}
