use super::*;

#[tokio::test]
async fn empty_task_list_keeps_header_and_invites_first_task() {
    let store = test_store_with_tasks(Vec::new()).await;

    let rendered = buffer_text(&render_task_list_buffer(&store, 80, 10));

    assert!(rendered.contains("TITLE"));
    assert!(rendered.contains("No tasks in this workspace"));
    assert!(rendered.contains("Add the first task to start building your queue."));
    assert!(rendered.contains("Add a task"));
}

#[tokio::test]
async fn empty_filtered_task_list_offers_to_clear_filters() {
    let mut store = test_store_with_tasks(Vec::new()).await;
    store.view_state.filter_modifiers.label = Some("blocked".to_string());

    let rendered = buffer_text(&render_task_list_buffer(&store, 64, 7));

    assert!(rendered.contains("No tasks match these filters"));
    assert!(rendered.contains("f c"));
    assert!(rendered.contains("Clear filters"));
    assert!(!rendered.contains("No tasks in this workspace"));
}

#[tokio::test]
async fn empty_epics_view_teaches_closed_filter() {
    let mut store = test_store_with_tasks(Vec::new()).await;
    store.view_state.query = TaskQuery::Epics;

    let state = crate::tui::ui::empty_state::task_empty_state(&store);

    assert_eq!(state.title, "No open epics");
    assert_eq!(
        state.action.map(|action| action.action),
        Some(crate::tui::event::Action::ToggleClosedFilter)
    );

    store.view_state.filter_modifiers.closed = ClosedTaskVisibility::Only;
    let state = crate::tui::ui::empty_state::task_empty_state(&store);
    assert_eq!(state.title, "No closed epics");
}

#[tokio::test]
async fn empty_state_classifies_named_and_specialized_surfaces() {
    use crate::tui::ui::empty_state::{
        EmptyStateReason, column_board_empty_state, recent_actions_empty_state,
        recurrence_empty_state, task_empty_state,
    };

    let mut store = test_store_with_tasks(Vec::new()).await;
    let named_views = [
        TaskQuery::Queue,
        TaskQuery::All,
        TaskQuery::Open,
        TaskQuery::Inbox,
        TaskQuery::Active,
        TaskQuery::Backlog,
        TaskQuery::Todo,
        TaskQuery::Done,
        TaskQuery::Upcoming,
        TaskQuery::Conflicts,
        TaskQuery::Epics,
    ];
    for view in named_views {
        store.view_state.query = view;
        let state = task_empty_state(&store);
        assert!(!state.title.is_empty(), "missing title for {view:?}");
        assert!(state.action.is_some(), "missing action for {view:?}");
    }

    store.view_state.scope = TaskScope::Project("app".to_string());
    store.view_state.query = TaskQuery::Queue;
    assert_eq!(task_empty_state(&store).title, "No tasks in this project");

    store.view_state.scope = TaskScope::Workspace;
    store.view_state.query = TaskQuery::Search;
    store.view_state.projection_origin = TaskProjectionOrigin::SearchPrompt;
    assert_eq!(task_empty_state(&store).title, "Search tasks");

    store.view_state.projection_origin = TaskProjectionOrigin::Search {
        query: "missing".to_string(),
        task_ids: Vec::new(),
    };
    assert_eq!(
        task_empty_state(&store).reason,
        EmptyStateReason::NoSearchResults
    );

    store.view_state.projection_origin = TaskProjectionOrigin::Search {
        query: "matched".to_string(),
        task_ids: vec![crate::test_support::task_id("matched")],
    };
    store.view_state.filter_modifiers.label = Some("hidden".to_string());
    assert_eq!(
        task_empty_state(&store).reason,
        EmptyStateReason::NoFilterMatches
    );

    store.view_state.projection_origin = TaskProjectionOrigin::NamedView;
    store.view_state.filter_modifiers.label = None;
    store.view_state.filter_modifiers.deleted_only = true;
    assert_eq!(
        task_empty_state(&store).reason,
        EmptyStateReason::NoDeletedTasks
    );

    store.view_state.filter_modifiers = Default::default();
    store.view_state.query = TaskQuery::Queue;
    store.counts.upcoming = 2;
    assert_eq!(
        task_empty_state(&store).reason,
        EmptyStateReason::DeferredTasks
    );

    store.view_state.query = TaskQuery::Recurring;
    store.view_state.recurring.lifecycle = crate::query::RecurrenceSeriesLifecycleFilter::Stopped;
    assert_eq!(
        recurrence_empty_state(&store).reason,
        EmptyStateReason::RecurrenceLifecycle
    );

    store.view_state.query = TaskQuery::RecentActions;
    assert_eq!(
        recent_actions_empty_state(&store).reason,
        EmptyStateReason::RecentActions
    );

    store.tasks.push(task_list_item("Unassigned status"));
    store.set_task_columns(vec![crate::config::TaskColumnConfig {
        name: "Inbox".to_string(),
        statuses: vec!["inbox".to_string()],
    }]);
    assert_eq!(
        column_board_empty_state(&store).reason,
        EmptyStateReason::ColumnConfiguration
    );
    store.tasks.clear();

    store.fail_next_refresh();
    store.refresh(None).await.unwrap_err();
    assert_eq!(
        recent_actions_empty_state(&store).reason,
        EmptyStateReason::LoadFailed
    );
}

#[tokio::test]
async fn empty_task_list_adapts_to_short_and_narrow_bodies() {
    let store = test_store_with_tasks(Vec::new()).await;

    let one_body_row = buffer_text(&render_task_list_buffer(&store, 20, 2));
    let header_only = buffer_text(&render_task_list_buffer(&store, 20, 1));

    assert!(one_body_row.contains("add task"));
    assert!(!one_body_row.contains("No tasks in this workspace"));
    assert!(!header_only.contains("add task"));
}

#[tokio::test]
async fn populated_task_list_preserves_rows_without_empty_prompt() {
    let store = test_store_with_tasks(vec![task_list_item("Ship the release")]).await;

    let rendered = buffer_text(&render_task_list_buffer(&store, 80, 7));

    assert!(rendered.contains("Ship the release"));
    assert!(!rendered.contains("No tasks in this workspace"));
    assert!(!rendered.contains("Add the first task to start building your queue."));
}
