//! Process-manager panel: list of background tasks + per-task log view.
//!
//! Issue #19. This file currently exists only to host the failing tests
//! that drive the implementation. The next commit fills in the model
//! types and the GTK widget.

// --- Tests ---------------------------------------------------------------
//
// Spec-driven TDD: these tests reference symbols (`TasksModel`,
// `view_model_for`, `TasksPanel`, …) that do not exist yet. The
// failing-tests commit captures the contract; the implementation commit
// makes them pass.

#[cfg(test)]
mod tests {
    use super::*;
    use desktop_assistant_api_model as api;

    fn running_task(id: &str, conv: &str) -> api::TaskView {
        api::TaskView {
            id: api::TaskId(id.into()),
            kind: api::TaskKind::Standalone {
                name: "agent".into(),
                conversation_id: conv.into(),
            },
            status: api::TaskStatus::Running,
            started_at: 1_700_000_000_000,
            ended_at: None,
            last_error: None,
            parent: None,
            children: vec![],
            title: format!("Task {id}"),
            progress_hint: None,
        }
    }

    fn log_entry(seq: u64, message: &str) -> api::TaskLogEntry {
        api::TaskLogEntry {
            seq,
            timestamp: 1_700_000_000_000,
            level: api::LogLevel::Info,
            category: api::LogCategory::Status,
            message: message.into(),
            data: None,
        }
    }

    #[test]
    fn task_row_view_model_from_task_view() {
        let t = running_task("t-1", "conv-9");
        let now = t.started_at + 12_345;
        let vm = view_model_for(&t, now);

        assert_eq!(vm.id, "t-1");
        assert_eq!(vm.title, "Task t-1");
        assert_eq!(vm.status_class, "task-dot-running");
        assert_eq!(vm.age_text, "12s");
        assert_eq!(vm.conversation_id.as_deref(), Some("conv-9"));
    }

    #[test]
    fn task_row_view_model_completed_uses_ended_at_not_now() {
        let mut t = running_task("t-end", "c");
        t.status = api::TaskStatus::Completed;
        t.ended_at = Some(t.started_at + 5_000);
        let now = t.started_at + 9_999_000;
        let vm = view_model_for(&t, now);
        assert_eq!(vm.status_class, "task-dot-completed");
        assert_eq!(vm.age_text, "5s");
    }

    #[test]
    fn task_row_view_model_clamps_negative_age() {
        let mut t = running_task("t-clock", "c");
        t.started_at = 1_000_000;
        let now = 999_000;
        let vm = view_model_for(&t, now);
        assert_eq!(vm.age_text, "0s");
    }

    #[test]
    fn task_row_view_model_formats_minutes_and_hours() {
        let mut t = running_task("t-mins", "c");
        t.started_at = 0;
        assert_eq!(view_model_for(&t, 90_000).age_text, "1m 30s");
        assert_eq!(view_model_for(&t, 3_725_000).age_text, "1h 2m");
    }

    #[test]
    fn internal_msg_task_started_inserts_row_in_store() {
        let mut model = TasksModel::new();
        assert_eq!(model.len(), 0);
        model.upsert(running_task("t-1", "c"));
        assert_eq!(model.len(), 1);
        assert_eq!(model.get(0).unwrap().id.0, "t-1");
    }

    #[test]
    fn business_outcome_user_can_see_their_running_standalone_agent_appear_in_real_time() {
        let mut model = TasksModel::new();
        model.upsert(running_task("agent-1", "conv-1"));
        let view = model.iter().collect::<Vec<_>>();
        assert_eq!(view.len(), 1);
        assert_eq!(view[0].title, "Task agent-1");
        assert_eq!(view[0].status, api::TaskStatus::Running);
    }

    #[test]
    fn internal_msg_task_log_appended_appends_to_buffer() {
        let mut model = TasksModel::new();
        model.upsert(running_task("t-log", "c"));
        model.append_log("t-log", log_entry(1, "step 1"));
        model.append_log("t-log", log_entry(2, "step 2"));
        let logs = model.logs_for("t-log");
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0].message, "step 1");
        assert_eq!(logs[1].message, "step 2");
    }

    #[test]
    fn rapid_event_burst_preserves_seq_ordering() {
        let mut model = TasksModel::new();
        model.append_log("t", log_entry(3, "third"));
        model.append_log("t", log_entry(1, "first"));
        model.append_log("t", log_entry(2, "second"));
        let logs = model.logs_for("t");
        assert_eq!(
            logs.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn duplicate_log_seq_does_not_duplicate_entries() {
        let mut model = TasksModel::new();
        model.append_log("t", log_entry(1, "first"));
        model.append_log("t", log_entry(1, "first"));
        assert_eq!(model.logs_for("t").len(), 1);
    }

    #[test]
    fn log_buffer_is_bounded() {
        let mut model = TasksModel::new();
        for i in 0..(LOG_BUFFER_MAX as u64 + 50) {
            model.append_log("t", log_entry(i, "x"));
        }
        assert_eq!(model.logs_for("t").len(), LOG_BUFFER_MAX);
        let logs = model.logs_for("t");
        assert!(logs.first().unwrap().seq >= 50);
    }

    #[test]
    fn apply_progress_updates_existing_task() {
        let mut model = TasksModel::new();
        model.upsert(running_task("t-1", "c"));
        model.apply_progress("t-1", Some("step 2/4".into()));
        assert_eq!(
            model.get(0).unwrap().progress_hint.as_deref(),
            Some("step 2/4")
        );
    }

    #[test]
    fn apply_progress_for_unknown_task_is_noop() {
        let mut model = TasksModel::new();
        model.apply_progress("ghost", Some("hint".into()));
        assert_eq!(model.len(), 0);
    }

    #[test]
    fn apply_completion_transitions_status() {
        let mut model = TasksModel::new();
        model.upsert(running_task("t-end", "c"));
        model.apply_completion(
            "t-end",
            api::TaskStatus::Failed,
            Some("boom".into()),
            1_700_000_999_000,
        );
        let t = model.get(0).unwrap();
        assert_eq!(t.status, api::TaskStatus::Failed);
        assert_eq!(t.last_error.as_deref(), Some("boom"));
        assert_eq!(t.ended_at, Some(1_700_000_999_000));
    }

    #[test]
    fn replace_all_clears_orphan_log_buffers() {
        let mut model = TasksModel::new();
        model.upsert(running_task("old", "c"));
        model.append_log("old", log_entry(1, "x"));
        assert_eq!(model.logs_for("old").len(), 1);

        model.replace_all(vec![running_task("new", "c")]);
        assert_eq!(model.len(), 1);
        assert_eq!(model.logs_for("old").len(), 0);
    }

    #[test]
    fn upsert_replaces_existing_task_in_place() {
        let mut model = TasksModel::new();
        model.upsert(running_task("t-1", "c"));
        let mut updated = running_task("t-1", "c");
        updated.title = "renamed".into();
        model.upsert(updated);
        assert_eq!(model.len(), 1);
        assert_eq!(model.get(0).unwrap().title, "renamed");
    }

    #[test]
    fn newest_task_appears_at_top_of_list() {
        let mut model = TasksModel::new();
        model.upsert(running_task("older", "c"));
        model.upsert(running_task("newer", "c"));
        assert_eq!(model.get(0).unwrap().id.0, "newer");
        assert_eq!(model.get(1).unwrap().id.0, "older");
    }

    #[test]
    fn malformed_task_kind_does_not_crash_view_model() {
        let t = api::TaskView {
            id: api::TaskId("t".into()),
            kind: api::TaskKind::Conversation {
                conversation_id: String::new(),
            },
            status: api::TaskStatus::Running,
            started_at: 0,
            ended_at: None,
            last_error: None,
            parent: None,
            children: vec![],
            title: "t".into(),
            progress_hint: None,
        };
        let vm = view_model_for(&t, 1_000);
        assert_eq!(vm.conversation_id.as_deref(), Some(""));
    }

    #[test]
    fn format_log_line_includes_level_and_category() {
        let mut e = log_entry(1, "calling search");
        e.level = api::LogLevel::Warn;
        e.category = api::LogCategory::ToolCall;
        let line = format_log_line(&e);
        assert!(line.contains("WARN"));
        assert!(line.contains("tool"));
        assert!(line.contains("calling search"));
    }

    fn cancel_id_for_selection(model: &TasksModel, selected: Option<usize>) -> Option<String> {
        selected.and_then(|i| model.get(i)).map(|t| t.id.0.clone())
    }

    fn open_conversation_id_for_selection(
        model: &TasksModel,
        selected: Option<usize>,
    ) -> Option<String> {
        selected
            .and_then(|i| model.get(i))
            .and_then(|t| conversation_id_for(&t.kind))
    }

    #[test]
    fn cancel_button_emits_cancel_command_for_selection() {
        let mut model = TasksModel::new();
        model.upsert(running_task("first", "c1"));
        model.upsert(running_task("second", "c2"));
        assert_eq!(
            cancel_id_for_selection(&model, Some(0)).as_deref(),
            Some("second")
        );
        assert_eq!(
            cancel_id_for_selection(&model, Some(1)).as_deref(),
            Some("first")
        );
    }

    #[test]
    fn cancel_with_no_selection_is_noop() {
        let model = TasksModel::new();
        assert_eq!(cancel_id_for_selection(&model, None), None);
    }

    #[test]
    fn cancel_with_stale_index_is_noop() {
        let model = TasksModel::new();
        assert_eq!(cancel_id_for_selection(&model, Some(42)), None);
    }

    #[test]
    fn open_conversation_routes_to_correct_id() {
        let mut model = TasksModel::new();
        model.upsert(running_task("agent", "the-conversation"));
        assert_eq!(
            open_conversation_id_for_selection(&model, Some(0)).as_deref(),
            Some("the-conversation")
        );
    }

    #[test]
    fn open_conversation_with_no_selection_is_noop() {
        let model = TasksModel::new();
        assert_eq!(open_conversation_id_for_selection(&model, None), None);
    }
}
