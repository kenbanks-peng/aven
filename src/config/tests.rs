use super::*;

fn load_config(text: &str) -> Result<AppConfig> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("config.yaml");
    fs::write(&path, text)?;
    AppConfig::load_from_path(&path)
}

#[test]
fn sync_server_resolution_uses_flag_environment_then_config() {
    let mut config = AppConfig::default();
    config.sync.server_url = Some("https://configured.example.test".to_string());

    assert_eq!(
        resolve_sync_server_from(
            Some("https://explicit.example.test"),
            Some("https://environment.example.test"),
            &config,
        )
        .unwrap(),
        "https://explicit.example.test"
    );
    assert_eq!(
        resolve_sync_server_from(None, Some("https://environment.example.test"), &config).unwrap(),
        "https://environment.example.test"
    );
    assert_eq!(
        resolve_sync_server_from(None, None, &config).unwrap(),
        "https://configured.example.test"
    );
}

#[test]
fn tilde_paths_expand_from_home() {
    let home = dirs::home_dir().expect("home directory");

    assert_eq!(
        expand_tilde(Path::new("~/work")).unwrap(),
        home.join("work")
    );
    assert_eq!(
        expand_tilde(Path::new("~someone/work")).unwrap(),
        PathBuf::from("~someone/work")
    );
    assert_eq!(
        expand_tilde(Path::new("relative/work")).unwrap(),
        PathBuf::from("relative/work")
    );
}

#[test]
fn default_columns_cover_every_status_once() {
    let config = AppConfig::default();

    config.validate().unwrap();
    assert_eq!(
        config
            .tui
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>(),
        ["Inbox", "Backlog", "Todo", "Active", "Done"]
    );
}

#[test]
fn sidebar_views_default_to_existing_order() {
    let expected = vec![
        SidebarView::Queue,
        SidebarView::Ready,
        SidebarView::Blocked,
        SidebarView::Overdue,
        SidebarView::All,
        SidebarView::Open,
        SidebarView::Inbox,
        SidebarView::Active,
        SidebarView::Backlog,
        SidebarView::Todo,
        SidebarView::Upcoming,
        SidebarView::Done,
        SidebarView::Conflicts,
        SidebarView::Epics,
        SidebarView::Recurring,
        SidebarView::RecentActions,
        SidebarView::Search,
    ];

    assert_eq!(AppConfig::default().tui.sidebar.views, expected);
    assert_eq!(load_config("{}\n").unwrap().tui.sidebar.views, expected);
    assert_eq!(
        load_config("tui:\n  sidebar: {}\n")
            .unwrap()
            .tui
            .sidebar
            .views,
        expected
    );
}

#[test]
fn sidebar_views_load_in_configured_order_and_support_empty_lists() {
    let configured =
        load_config("tui:\n  sidebar:\n    views: [search, recent_actions, queue]\n").unwrap();
    assert_eq!(
        configured.tui.sidebar.views,
        [
            SidebarView::Search,
            SidebarView::RecentActions,
            SidebarView::Queue,
        ]
    );

    let empty = load_config("tui:\n  sidebar:\n    views: []\n").unwrap();
    assert!(empty.tui.sidebar.views.is_empty());
}

#[test]
fn sidebar_views_reject_duplicates() {
    let error = load_config("tui:\n  sidebar:\n    views: [queue, done, queue]\n").unwrap_err();

    assert!(format!("{error:#}").contains("tui.sidebar.views contains duplicate view Queue"));
}

#[test]
fn sidebar_views_reject_unknown_names() {
    let error = load_config("tui:\n  sidebar:\n    views: [queue, someday]\n").unwrap_err();

    assert!(format!("{error:#}").contains("unknown variant `someday`"));
}

#[test]
fn table_columns_default_and_round_trip() {
    for yaml in ["{}", "tui: {}", "tui:\n  table: {}"] {
        assert_eq!(
            load_config(yaml).unwrap().tui.table.columns,
            TableColumn::DEFAULT
        );
    }
    let config = load_config("tui:\n  table:\n    columns: [status, priority, ref]").unwrap();
    assert_eq!(
        config.tui.table.columns,
        [TableColumn::Status, TableColumn::Priority, TableColumn::Ref]
    );
    let text = serde_yaml::to_string(&config).unwrap();
    assert_eq!(
        load_config(&text).unwrap().tui.table.columns,
        config.tui.table.columns
    );
    assert_eq!(config.tui.columns, default_task_columns());
}

#[test]
fn compact_status_defaults_off_and_loads_under_the_table_settings() {
    for yaml in ["{}", "tui: {}", "tui:\n  table: {}"] {
        assert!(!load_config(yaml).unwrap().tui.table.compact_status);
    }
    assert!(!TaskTableConfig::default().compact_status);

    let config = load_config("tui:\n  table:\n    compact_status: true\n").unwrap();
    assert!(config.tui.table.compact_status);
    assert_eq!(config.tui.table.columns, default_table_columns());

    let text = serde_yaml::to_string(&config).unwrap();
    assert!(load_config(&text).unwrap().tui.table.compact_status);
}

#[test]
fn due_column_is_opt_in_and_independent_of_the_time_column() {
    assert!(!default_table_columns().contains(&TableColumn::Due));

    let config = load_config("tui:\n  table:\n    columns: [title, due]").unwrap();
    assert_eq!(
        config.tui.table.columns,
        [TableColumn::Title, TableColumn::Due]
    );
    assert_eq!(TableColumn::Due.name(), "due");

    let text = serde_yaml::to_string(&config).unwrap();
    assert_eq!(
        load_config(&text).unwrap().tui.table.columns,
        config.tui.table.columns
    );

    let with_time = load_config("tui:\n  table:\n    columns: [title, due, time]").unwrap();
    assert_eq!(
        with_time.tui.table.columns,
        [TableColumn::Title, TableColumn::Due, TableColumn::Time]
    );
}

#[test]
fn table_columns_reject_empty_duplicate_and_unknown_columns() {
    for (columns, expected) in [
        ("[]", "tui.table.columns must include at least one column"),
        (
            "[ref, title, labels, metadata, project, status, priority, ref]",
            "tui.table.columns contains duplicate column ref",
        ),
        (
            "[title, due, due]",
            "tui.table.columns contains duplicate column due",
        ),
        ("[robot]", "unknown variant `robot`"),
    ] {
        let error = load_config(&format!("tui:\n  table:\n    columns: {columns}")).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains(expected), "{message}");
    }
}

#[test]
fn table_columns_reject_legacy_column_order_instead_of_ignoring_it() {
    let error = load_config(
            "tui:\n  table:\n    column_order: [ref, title, labels, metadata, project, status, priority, time]",
        )
        .unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("unknown field `column_order`"),
        "{message}"
    );
    assert!(message.contains("columns"), "{message}");
}

#[test]
fn custom_columns_load_in_configured_order() {
    let config = load_config(
            "tui:\n  columns:\n    - name: Work\n      statuses: [active, todo]\n    - name: Later\n      statuses: [inbox, backlog]\n    - name: Closed\n      statuses: [done, canceled]\n",
        )
        .unwrap();

    assert_eq!(config.tui.columns[0].name, "Work");
    assert_eq!(config.tui.columns[0].statuses, ["active", "todo"]);
}

#[test]
fn empty_config_uses_default_columns() {
    assert_eq!(load_config("{}\n").unwrap().tui.columns.len(), 5);
}

#[test]
fn column_config_rejects_missing_statuses() {
    let error =
        load_config("tui:\n  columns:\n    - name: Current\n      statuses: [active, todo]\n")
            .unwrap_err();

    assert!(format!("{error:#}").contains("missing column statuses"));
}

#[test]
fn column_config_rejects_duplicate_statuses() {
    let error = load_config(
            "tui:\n  columns:\n    - name: One\n      statuses: [inbox, backlog, todo, active, done, canceled]\n    - name: Two\n      statuses: [active]\n",
        )
        .unwrap_err();

    assert!(format!("{error:#}").contains("duplicate column status active"));
}

#[test]
fn column_config_rejects_unknown_statuses() {
    let error = load_config(
            "tui:\n  columns:\n    - name: One\n      statuses: [inbox, backlog, todo, active, done, canceled, parked]\n",
        )
        .unwrap_err();

    assert!(format!("{error:#}").contains("unknown column status parked"));
}

#[test]
fn column_config_rejects_empty_columns_and_names() {
    let empty = load_config("tui:\n  columns: []\n").unwrap_err();
    assert!(format!("{empty:#}").contains("requires at least one column"));

    let unnamed = load_config(
            "tui:\n  columns:\n    - name: '  '\n      statuses: [inbox, backlog, todo, active, done, canceled]\n",
        )
        .unwrap_err();
    assert!(format!("{unnamed:#}").contains("column name must not be blank"));

    let no_statuses =
        load_config("tui:\n  columns:\n    - name: Empty\n      statuses: []\n").unwrap_err();
    assert!(format!("{no_statuses:#}").contains("must include at least one status"));
}

#[test]
fn custom_tui_commands_deserialize_and_validate() {
    let config = load_config(
            "tui:\n  commands:\n    - name: dispatch\n      aliases: [custom-dispatch]\n      description: Dispatch selected task\n      program: ~/bin/dispatch\n      args: [--tmux]\n      keys: [z d, D]\n      detail_keys: [z D]\n      requires: selected-task\n      execution: wait\n      on_success: quit\n",
        )
        .unwrap();
    let command = &config.tui.commands[0];

    assert_eq!(command.name, "dispatch");
    assert_eq!(command.aliases, ["custom-dispatch"]);
    assert_eq!(command.args, ["--tmux"]);
    assert_eq!(command.keys, ["z d", "D"]);
    assert_eq!(command.detail_keys.as_ref().unwrap(), &["z D".to_string()]);
    assert_eq!(command.target, CustomTuiCommandTarget::Focused);
    assert_eq!(command.execution, CustomTuiCommandExecution::Wait);
    assert_eq!(command.on_success, CustomTuiCommandSuccess::Quit);

    let terminal = load_config(
            "tui:\n  commands:\n    - name: agent\n      description: Interactive agent\n      program: agent\n      execution: terminal\n      on_success: refresh-and-quit\n",
        )
        .unwrap();
    assert_eq!(
        terminal.tui.commands[0].execution,
        CustomTuiCommandExecution::Terminal
    );
    assert_eq!(
        terminal.tui.commands[0].on_success,
        CustomTuiCommandSuccess::RefreshAndQuit
    );
}

#[test]
fn custom_tui_command_static_execution_settings_deserialize_with_compatible_defaults() {
    let configured = load_config(
            "tui:\n  commands:\n    - name: dispatch\n      description: Dispatch\n      program: dispatch\n      cwd: ~/code/tools\n      env:\n        PROFILE: staging\n        NO_COLOR: '1'\n      timeout_seconds: 30\n",
        )
        .unwrap();
    let command = &configured.tui.commands[0];
    assert_eq!(command.cwd.as_deref(), Some(Path::new("~/code/tools")));
    assert_eq!(command.env["PROFILE"], "staging");
    assert_eq!(command.env["NO_COLOR"], "1");
    assert_eq!(command.timeout_seconds, Some(30));

    let defaulted = load_config(
            "tui:\n  commands:\n    - name: defaulted\n      description: Defaulted\n      program: dispatch\n",
        )
        .unwrap();
    let command = &defaulted.tui.commands[0];
    assert_eq!(command.cwd, None);
    assert!(command.env.is_empty());
    assert_eq!(command.timeout_seconds, None);
}

#[test]
fn custom_tui_commands_validate_timeout_bounds() {
    for value in [0, MAX_CUSTOM_COMMAND_TIMEOUT_SECONDS + 1] {
        let yaml = format!(
            "tui:\n  commands:\n    - name: dispatch\n      description: Dispatch\n      program: dispatch\n      timeout_seconds: {value}\n"
        );
        let error = load_config(&yaml).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("timeout_seconds must be between 1 and 86400"));
    }

    let yaml = format!(
        "tui:\n  commands:\n    - name: dispatch\n      description: Dispatch\n      program: dispatch\n      execution: background\n      timeout_seconds: {}\n",
        MAX_CUSTOM_COMMAND_TIMEOUT_SECONDS
    );
    assert!(load_config(&yaml).is_ok());
}

#[test]
fn custom_tui_commands_reject_invalid_environment_without_exposing_values() {
    for (environment, expected) in [
        ("        '': value\n", "invalid environment variable name"),
        (
            "        BAD=NAME: value\n",
            "invalid environment variable name",
        ),
        (
            "        \"BAD\\u0000NAME\": value\n",
            "invalid environment variable name",
        ),
        (
            "        SAFE_NAME: \"secret-marker\\u0000suffix\"\n",
            "environment variable \"SAFE_NAME\" contains a NUL byte",
        ),
    ] {
        let yaml = format!(
            "tui:\n  commands:\n    - name: dispatch\n      description: Dispatch\n      program: dispatch\n      env:\n{environment}"
        );
        let error = load_config(&yaml).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains(expected), "{message}");
        assert!(!message.contains("secret-marker"), "{message}");
        assert!(!message.contains("suffix"), "{message}");
    }
}

#[test]
fn custom_tui_commands_reject_unknown_fields_with_the_typo_location() {
    let error = load_config(
            "tui:\n  commands:\n    - name: dispatch\n      description: Dispatch\n      program: dispatch\n      timeot_seconds: 30\n",
        )
        .unwrap_err();
    let message = format!("{error:#}");

    assert!(
        message.contains("unknown field `timeot_seconds`"),
        "{message}"
    );
    assert!(message.contains("tui.commands[0]"), "{message}");
    assert!(message.contains("line 6"), "{message}");
}

#[test]
fn custom_tui_command_target_policies_and_legacy_requirements_are_compatible() {
    for (field, expected) in [
        ("target: none", CustomTuiCommandTarget::None),
        ("target: focused", CustomTuiCommandTarget::Focused),
        ("target: marked", CustomTuiCommandTarget::Marked),
        (
            "target: marked-or-focused",
            CustomTuiCommandTarget::MarkedOrFocused,
        ),
        ("requires: none", CustomTuiCommandTarget::None),
        ("requires: selected-task", CustomTuiCommandTarget::Focused),
    ] {
        let yaml = format!(
            "tui:\n  commands:\n    - name: dispatch\n      description: Dispatch\n      program: dispatch\n      {field}\n"
        );
        let config = load_config(&yaml).unwrap();
        assert_eq!(config.tui.commands[0].target, expected, "{field}");
    }

    let defaulted = load_config(
            "tui:\n  commands:\n    - name: dispatch\n      description: Dispatch\n      program: dispatch\n",
        )
        .unwrap();
    assert_eq!(
        defaulted.tui.commands[0].target,
        CustomTuiCommandTarget::Focused
    );
}

#[test]
fn custom_tui_commands_reject_target_with_legacy_requires() {
    let error = load_config(
            "tui:\n  commands:\n    - name: dispatch\n      description: Dispatch\n      program: dispatch\n      target: marked\n      requires: selected-task\n",
        )
        .unwrap_err();

    assert!(format!("{error:#}").contains("cannot supply both target and requires"));
}

#[test]
fn custom_tui_commands_serialize_target_policy() {
    let config = load_config(
            "tui:\n  commands:\n    - name: dispatch\n      description: Dispatch\n      program: dispatch\n      requires: none\n",
        )
        .unwrap();
    let yaml = serde_yaml::to_string(&config).unwrap();

    assert!(yaml.contains("target: none"));
    assert!(!yaml.contains("requires:"));
}

#[test]
fn custom_tui_commands_reject_invalid_names_and_collisions() {
    for yaml in [
        "tui:\n  commands:\n    - name: ':dispatch'\n      description: Dispatch\n      program: dispatch\n",
        "tui:\n  commands:\n    - name: quit\n      description: Dispatch\n      program: dispatch\n",
        "tui:\n  commands:\n    - name: dispatch\n      aliases: [dispatch]\n      description: Dispatch\n      program: dispatch\n",
        "tui:\n  commands:\n    - name: dispatch\n      aliases: [same]\n      description: Dispatch\n      program: dispatch\n    - name: other\n      aliases: [same]\n      description: Other\n      program: other\n",
    ] {
        assert!(load_config(yaml).is_err(), "accepted {yaml}");
    }
}

#[test]
fn custom_tui_command_success_policies_deserialize() {
    for (value, expected) in [
        ("stay", CustomTuiCommandSuccess::Stay),
        ("refresh", CustomTuiCommandSuccess::Refresh),
        ("quit", CustomTuiCommandSuccess::Quit),
        ("refresh-and-quit", CustomTuiCommandSuccess::RefreshAndQuit),
    ] {
        let yaml = format!(
            "tui:\n  commands:\n    - name: dispatch\n      description: Dispatch\n      program: dispatch\n      on_success: {value}\n"
        );
        let config = load_config(&yaml).unwrap();
        assert_eq!(config.tui.commands[0].on_success, expected, "{value}");
    }
}

#[test]
fn custom_tui_commands_reject_blank_fields_and_non_stay_background_policies() {
    for yaml in [
        "tui:\n  commands:\n    - name: dispatch\n      description: '  '\n      program: dispatch\n",
        "tui:\n  commands:\n    - name: dispatch\n      description: Dispatch\n      program: ''\n",
        "tui:\n  commands:\n    - name: dispatch\n      description: Dispatch\n      program: dispatch\n      execution: background\n      on_success: refresh\n",
        "tui:\n  commands:\n    - name: dispatch\n      description: Dispatch\n      program: dispatch\n      execution: background\n      on_success: quit\n",
        "tui:\n  commands:\n    - name: dispatch\n      description: Dispatch\n      program: dispatch\n      execution: background\n      on_success: refresh-and-quit\n",
    ] {
        assert!(load_config(yaml).is_err(), "accepted {yaml}");
    }

    let stay = load_config(
            "tui:\n  commands:\n    - name: dispatch\n      description: Dispatch\n      program: dispatch\n      execution: background\n      on_success: stay\n",
        )
        .unwrap();
    assert_eq!(
        stay.tui.commands[0].on_success,
        CustomTuiCommandSuccess::Stay
    );
}

#[test]
fn custom_tui_commands_validate_contextual_keybindings() {
    let inherited = load_config(
            "tui:\n  commands:\n    - name: dispatch\n      description: Dispatch\n      program: dispatch\n      keys: [z d]\n",
        )
        .unwrap();
    assert_eq!(inherited.tui.commands[0].detail_keys, None);

    let detail_disabled = load_config(
            "tui:\n  commands:\n    - name: dispatch\n      description: Dispatch\n      program: dispatch\n      keys: [z d]\n      detail_keys: []\n",
        )
        .unwrap();
    assert_eq!(detail_disabled.tui.commands[0].detail_keys, Some(vec![]));

    for yaml in [
        "tui:\n  commands:\n    - name: dispatch\n      description: Dispatch\n      program: dispatch\n      keys: [q]\n",
        "tui:\n  commands:\n    - name: dispatch\n      description: Dispatch\n      program: dispatch\n      keys: ['']\n",
        "tui:\n  commands:\n    - name: dispatch\n      description: Dispatch\n      program: dispatch\n      keys: [Esc]\n",
        "tui:\n  commands:\n    - name: dispatch\n      description: Dispatch\n      program: dispatch\n      keys: [z unknown]\n",
        "tui:\n  commands:\n    - name: dispatch\n      description: Dispatch\n      program: dispatch\n      keys: [z d a b c]\n",
        "tui:\n  commands:\n    - name: dispatch\n      description: Dispatch\n      program: dispatch\n      keys: [z d]\n      detail_keys: [s]\n",
        "tui:\n  commands:\n    - name: dispatch\n      description: Dispatch\n      program: dispatch\n      keys: [z d]\n    - name: other\n      description: Other\n      program: other\n      keys: [z d]\n",
    ] {
        assert!(load_config(yaml).is_err(), "accepted {yaml}");
    }
}

#[test]
fn project_override_workspace_ids_are_validated() {
    let valid = load_config(
            "project:\n  overrides:\n    - workspace_id: 0123456789ABCDEF\n      project: app\n      paths: []\n",
        )
        .unwrap();
    assert_eq!(
        valid.project.overrides[0]
            .workspace_id
            .as_ref()
            .unwrap()
            .as_str(),
        "0123456789ABCDEF"
    );

    let invalid = load_config(
            "project:\n  overrides:\n    - workspace_id: invalid\n      project: app\n      paths: []\n",
        )
        .unwrap_err();
    assert!(format!("{invalid:#}").contains("workspace ID must be"));
}

#[test]
fn default_columns_round_trip() {
    let config = AppConfig::default();
    let yaml = serde_yaml::to_string(&config).unwrap();
    let loaded: AppConfig = serde_yaml::from_str(&yaml).unwrap();

    loaded.validate().unwrap();
    assert_eq!(loaded.tui.columns, config.tui.columns);
}

#[test]
fn debug_database_resolution_requires_an_explicit_path() {
    let config = AppConfig::default();
    let error = resolve_db_path_from(None, None, None, &config, true).unwrap_err();

    assert!(format!("{error:#}").contains("debug-database-required"));
}

#[test]
fn debug_database_resolution_uses_dev_environment_path() {
    let config = AppConfig::default();
    let dev_db = PathBuf::from("/tmp/aven-dev.sqlite");

    assert_eq!(
        resolve_db_path_from(None, None, Some(dev_db.clone()), &config, true).unwrap(),
        dev_db
    );
}

#[test]
fn debug_database_resolution_prefers_dev_environment_path() {
    let config = AppConfig::default();
    let dev_db = PathBuf::from("/tmp/aven-dev.sqlite");
    let env_db = PathBuf::from("/tmp/aven-env.sqlite");

    assert_eq!(
        resolve_db_path_from(None, Some(env_db), Some(dev_db.clone()), &config, true,).unwrap(),
        dev_db
    );
}

#[test]
fn database_flag_overrides_debug_environment_path() {
    let config = AppConfig::default();
    let dev_db = Some(PathBuf::from("/tmp/aven-dev.sqlite"));
    let flag_db = PathBuf::from("/tmp/aven-flag.sqlite");

    assert_eq!(
        resolve_db_path_from(Some(flag_db.clone()), None, dev_db, &config, true).unwrap(),
        flag_db
    );
}

#[test]
fn release_database_resolution_ignores_dev_environment_path() {
    let mut config = AppConfig::default();
    config.local.db_path = Some(PathBuf::from("/tmp/configured.sqlite"));

    assert_eq!(
        resolve_db_path_from(
            None,
            None,
            Some(PathBuf::from("/tmp/aven-dev.sqlite")),
            &config,
            false,
        )
        .unwrap(),
        PathBuf::from("/tmp/configured.sqlite")
    );
}

#[test]
fn update_automatic_checks_parse_from_config() {
    let disabled: AppConfig = serde_yaml::from_str("update:\n  automatic_checks: false\n").unwrap();
    let default: AppConfig = serde_yaml::from_str("{}\n").unwrap();

    assert!(!disabled.update.automatic_checks);
    assert!(default.update.automatic_checks);
}

#[test]
fn update_check_environment_disable_takes_precedence() {
    for value in [Some("1"), Some("true"), Some("YES")] {
        assert!(!automatic_update_checks_enabled(true, value));
    }
    assert!(!automatic_update_checks_enabled(false, None));
    assert!(automatic_update_checks_enabled(true, Some("false")));
}

#[test]
fn sync_disabled_flag_recognizes_true_values() {
    for value in [Some("1"), Some("true"), Some("yes")] {
        assert!(sync_disabled_value(value));
    }
}

#[test]
fn sync_disabled_flag_ignores_other_values() {
    for value in [None, Some("0"), Some("false"), Some("")] {
        assert!(!sync_disabled_value(value));
    }
}

#[test]
fn generated_config_includes_sync_opt_in_without_inactive_override() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");

    write_default_config(&path).unwrap();

    let text = fs::read_to_string(path).unwrap();
    assert!(text.contains("sync:\n  enabled: false\n"));
    assert!(!text.contains("disabled:"));
}

#[test]
fn runtime_disable_override_is_not_serialized() {
    let mut config = AppConfig::default();
    config.sync.enabled = true;
    config.sync.disable_override = true;

    let text = serde_yaml::to_string(&config).unwrap();

    assert!(text.contains("enabled: true"));
    assert!(!text.contains("disabled:"));
}

#[test]
fn effective_sync_predicates_distinguish_opt_in_and_override() {
    let mut disabled = AppConfig::default();
    disabled.sync.enabled = true;
    disabled.sync.disable_override = true;
    let mut enabled = AppConfig::default();
    enabled.sync.enabled = true;
    let unconfigured = AppConfig::default();

    assert!(!disabled.automatic_sync_is_enabled());
    assert!(enabled.automatic_sync_is_enabled());
    assert!(!unconfigured.automatic_sync_is_enabled());
    assert!(unconfigured.sync_is_allowed());
}

#[test]
fn disabled_sync_reports_actionable_error() {
    let mut config = AppConfig::default();
    config.sync.disable_override = true;
    let error = config.ensure_sync_allowed().unwrap_err();

    assert!(format!("{error:#}").contains("sync-disabled"));
    assert!(format!("{error:#}").contains("sync is disabled"));
}

#[test]
fn sync_without_disable_override_is_allowed() {
    AppConfig::default().ensure_sync_allowed().unwrap();
}

#[test]
fn resolves_blob_dir_from_db_path_and_config() {
    let db_path = PathBuf::from("/tmp/aven/db.sqlite");
    let config = AppConfig::default();
    assert_eq!(
        resolve_blob_dir(&db_path, &config).unwrap(),
        PathBuf::from("/tmp/aven/db.sqlite.blobs")
    );

    let mut config = AppConfig::default();
    config.local.blob_dir = Some(PathBuf::from("blobs"));
    assert_eq!(
        resolve_blob_dir(&db_path, &config).unwrap(),
        PathBuf::from("/tmp/aven/blobs")
    );

    config.local.blob_dir = Some(PathBuf::from("/var/aven/blobs"));
    assert_eq!(
        resolve_blob_dir(&db_path, &config).unwrap(),
        PathBuf::from("/var/aven/blobs")
    );
}

#[test]
fn generated_client_config_omits_server_attachment_settings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");

    write_default_config(&path).unwrap();

    let text = fs::read_to_string(&path).unwrap();
    assert!(!text.contains("server_grace_days"));
    assert!(!text.contains("server_workspace_quota_bytes"));
    let loaded = AppConfig::load_from_path(&path).unwrap();
    assert_eq!(
        loaded.local.attachment_lifecycle.server_grace_days,
        default_server_attachment_grace_days()
    );
    assert_eq!(
        loaded
            .local
            .attachment_lifecycle
            .server_workspace_quota_bytes,
        default_attachment_quota_bytes()
    );
}

#[test]
fn server_attachment_settings_remain_loadable_and_configurable() {
    let config = load_config(
            "local:\n  attachment_lifecycle:\n    server_grace_days: 12\n    server_workspace_quota_bytes: 345\n",
        )
        .unwrap();

    assert_eq!(config.local.attachment_lifecycle.server_grace_days, 12);
    assert_eq!(
        config
            .local
            .attachment_lifecycle
            .server_workspace_quota_bytes,
        345
    );
    let policy = config.local.attachment_lifecycle.server_policy();
    assert_eq!(
        policy.grace,
        std::time::Duration::from_secs(12 * 24 * 60 * 60)
    );
    assert_eq!(policy.quota_bytes, 345);
}

#[test]
fn local_inline_images_defaults_to_auto() {
    let config = AppConfig::default();

    assert_eq!(config.local.inline_images, InlineImagesConfig::Auto);
}

#[test]
fn local_image_optimization_defaults_to_off() {
    let config = AppConfig::default();

    assert_eq!(
        config.local.image_optimization,
        ImageOptimizationConfig::Off
    );
    assert!(!config.local.image_optimization.optimizes_pasted_images());
    assert!(!config.local.image_optimization.optimizes_file_attachments());
    let yaml = serde_yaml::to_string(&config).unwrap();
    assert!(yaml.contains("image_optimization: off"));

    for (value, expected) in [
        ("paste", ImageOptimizationConfig::Paste),
        ("on", ImageOptimizationConfig::On),
    ] {
        let parsed: AppConfig =
            serde_yaml::from_str(&format!("local:\n  image_optimization: {value}\n")).unwrap();
        assert_eq!(parsed.local.image_optimization, expected);
    }
}
