use super::editors::{MultilineInputState, PickerState, TagComboboxState};
use crate::tui::authoring::{
    AddTaskPriorityChoice, AddTaskStatusChoice, AddTaskStep, PendingTaskAttachmentSummary,
    automatic_add_task_status, derived_add_task_status,
};
use crate::tui::overlay::text_input::LineEdit;
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScheduleEditorMode {
    Once,
    Repeat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScheduleEditorField {
    Mode,
    Available,
    Due,
    Repeat,
    Time,
    DuePolicy,
    Starts,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScheduleEditorState {
    pub(crate) mode: ScheduleEditorMode,
    pub(crate) focus: ScheduleEditorField,
    pub(crate) available_at: LineEdit,
    pub(crate) due_on: LineEdit,
    pub(crate) repeat_rule: LineEdit,
    pub(crate) repeat_at: LineEdit,
    pub(crate) repeat_due: String,
    pub(crate) repeat_start_on: LineEdit,
    pub(crate) time_zone: String,
    pub(crate) template_locked: bool,
    pub(crate) preview: Vec<String>,
    pub(crate) error: Option<String>,
    pub(crate) validation_requested: bool,
}

impl ScheduleEditorState {
    pub(crate) fn fields(&self) -> &'static [ScheduleEditorField] {
        match self.mode {
            ScheduleEditorMode::Once => &[
                ScheduleEditorField::Mode,
                ScheduleEditorField::Available,
                ScheduleEditorField::Due,
            ],
            ScheduleEditorMode::Repeat if self.template_locked => &[
                ScheduleEditorField::Mode,
                ScheduleEditorField::Time,
                ScheduleEditorField::DuePolicy,
            ],
            ScheduleEditorMode::Repeat => &[
                ScheduleEditorField::Mode,
                ScheduleEditorField::Repeat,
                ScheduleEditorField::Time,
                ScheduleEditorField::DuePolicy,
                ScheduleEditorField::Starts,
            ],
        }
    }

    pub(crate) fn focus_next(&mut self, reverse: bool) {
        let fields = self.fields();
        let index = fields
            .iter()
            .position(|field| *field == self.focus)
            .unwrap_or(0);
        self.focus = if reverse {
            fields[index.checked_sub(1).unwrap_or(fields.len() - 1)]
        } else {
            fields[(index + 1) % fields.len()]
        };
    }

    pub(crate) fn cycle_mode(&mut self, _reverse: bool) {
        if self.template_locked {
            return;
        }
        self.mode = match self.mode {
            ScheduleEditorMode::Once => ScheduleEditorMode::Repeat,
            ScheduleEditorMode::Repeat => ScheduleEditorMode::Once,
        };
        self.focus = ScheduleEditorField::Mode;
        self.validation_requested = false;
        self.refresh();
    }

    pub(crate) fn refresh(&mut self) {
        self.preview.clear();
        let error = match self.mode {
            ScheduleEditorMode::Once => {
                let available = if self.available_at.text.trim().is_empty() {
                    Ok(String::new())
                } else {
                    crate::time_input::parse_available_at_input(&self.available_at.text)
                };
                available
                    .and_then(|_| {
                        if self.due_on.text.trim().is_empty() {
                            Ok(String::new())
                        } else {
                            crate::time_input::parse_due_on_input(&self.due_on.text)
                        }
                    })
                    .err()
                    .map(|error| format!("{error:#}"))
            }
            ScheduleEditorMode::Repeat => {
                let repeat_at = Some(self.repeat_at.text.trim()).filter(|value| !value.is_empty());
                let starts_on =
                    Some(self.repeat_start_on.text.trim()).filter(|value| !value.is_empty());
                match crate::recurrence_input::canonical_rule_input(&self.repeat_rule.text)
                    .and_then(|rule| {
                        let Some(rule) = rule else {
                            anyhow::bail!(crate::recurrence_input::rule_guidance());
                        };
                        crate::commands::recurrence_schedule(
                            &rule,
                            repeat_at,
                            Some(&self.repeat_due),
                            Some(self.time_zone.trim()).filter(|value| !value.is_empty()),
                            starts_on,
                        )
                    }) {
                    Ok(schedule) => {
                        let zone = schedule
                            .timezone
                            .as_str()
                            .parse::<chrono_tz::Tz>()
                            .expect("validated recurrence time zone parses");
                        let from = schedule
                            .start_on
                            .max(Utc::now().with_timezone(&zone).date_naive());
                        self.preview = schedule
                            .slots_on_or_after(from)
                            .take(3)
                            .map(|date| date.format("%a %b %-d %Y").to_string())
                            .collect();
                        None
                    }
                    Err(error) => Some(format!("{error:#}")),
                }
            }
        };
        self.error = self.validation_requested.then_some(error).flatten();
    }

    pub(crate) fn validate_current_field(&mut self) {
        if !matches!(
            self.focus,
            ScheduleEditorField::Mode | ScheduleEditorField::DuePolicy
        ) {
            self.validate();
        }
    }

    pub(crate) fn validate(&mut self) {
        self.validation_requested = true;
        self.refresh();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AddTaskMode {
    Compose,
    Schedule(ScheduleEditorState),
    Picker {
        field: AddTaskStep,
        state: PickerState,
    },
    Labels(TagComboboxState),
    Help {
        scroll: u16,
    },
    ConfirmDiscard,
}

impl AddTaskMode {
    pub(crate) fn expands_composer(&self) -> bool {
        match self {
            Self::Help { .. } => true,
            Self::Compose
            | Self::Schedule(_)
            | Self::Picker { .. }
            | Self::Labels(_)
            | Self::ConfirmDiscard => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AddTaskState {
    pub(crate) custom_metadata: Vec<aven_core::metadata::TaskMetadataInput>,
    pub(crate) title: LineEdit,
    pub(crate) description: MultilineInputState,
    pub(crate) focus: AddTaskStep,
    pub(crate) project: String,
    pub(crate) inferred_project: Option<String>,
    pub(crate) selected_project: Option<String>,
    pub(crate) initial_project: Option<String>,
    pub(crate) status: AddTaskStatusChoice,
    pub(crate) priority: AddTaskPriorityChoice,
    pub(crate) labels: Vec<String>,
    pub(crate) is_epic: bool,
    pub(crate) create_more: bool,
    pub(crate) create_more_available: bool,
    pub(crate) available_at: LineEdit,
    pub(crate) due_on: LineEdit,
    pub(crate) schedule_input: LineEdit,
    pub(crate) schedule_error: Option<String>,
    pub(crate) schedule_validation_requested: bool,
    pub(crate) attachments: Vec<PendingTaskAttachmentSummary>,
    pub(crate) selected_attachment: usize,
    pub(crate) recurrence_series_id: Option<aven_core::recurrence::RecurrenceSeriesId>,
    pub(crate) template_schedule: Option<aven_core::recurrence::RecurrenceSchedule>,
    pub(crate) repeat_rule: LineEdit,
    pub(crate) repeat_at: LineEdit,
    pub(crate) repeat_due: String,
    pub(crate) time_zone: String,
    pub(crate) repeat_start_on: LineEdit,
    pub(crate) schedule_expanded: bool,
    pub(crate) recurrence_preview: Vec<String>,
    pub(crate) recurrence_error: Option<String>,
    pub(crate) mode: AddTaskMode,
    pub(crate) title_error: bool,
}

impl AddTaskState {
    pub(crate) fn schedule_editor(&self, focus: ScheduleEditorField) -> ScheduleEditorState {
        let mode = if self.recurrence_enabled() {
            ScheduleEditorMode::Repeat
        } else {
            ScheduleEditorMode::Once
        };
        ScheduleEditorState {
            mode,
            focus,
            available_at: self.available_at.clone(),
            due_on: self.due_on.clone(),
            repeat_rule: self.repeat_rule.clone(),
            repeat_at: self.repeat_at.clone(),
            repeat_due: self.repeat_due.clone(),
            repeat_start_on: self.repeat_start_on.clone(),
            time_zone: self.time_zone.clone(),
            template_locked: self.template_schedule.is_some(),
            preview: self.recurrence_preview.clone(),
            validation_requested: self.recurrence_error.is_some(),
            error: self.recurrence_error.clone(),
        }
    }

    pub(crate) fn apply_schedule_input(&mut self) {
        match crate::schedule_input::parse_schedule_input(&self.schedule_input.text) {
            Ok(crate::schedule_input::ParsedScheduleInput::None) => {
                self.available_at = LineEdit::blank();
                self.due_on = LineEdit::blank();
                self.repeat_rule = LineEdit::blank();
                self.schedule_error = None;
            }
            Ok(crate::schedule_input::ParsedScheduleInput::Once {
                available_at,
                due_on,
            }) => {
                self.available_at = LineEdit::new(available_at);
                self.due_on = LineEdit::new(due_on);
                self.repeat_rule = LineEdit::blank();
                self.schedule_error = None;
            }
            Ok(crate::schedule_input::ParsedScheduleInput::Recurring {
                rule,
                available_time,
                due_policy,
                starts_on,
            }) if self.template_schedule.is_none() => {
                self.available_at = LineEdit::blank();
                self.due_on = LineEdit::blank();
                self.repeat_rule = LineEdit::new(rule);
                self.repeat_at = LineEdit::new(available_time);
                self.repeat_due = due_policy;
                if !starts_on.is_empty() {
                    self.repeat_start_on = LineEdit::new(starts_on);
                }
                self.schedule_error = None;
            }
            Ok(crate::schedule_input::ParsedScheduleInput::Recurring { .. }) => {
                self.schedule_error =
                    Some("The repeat rule and start date are fixed for this template".to_string());
            }
            Err(error) => self.schedule_error = Some(format!("{error:#}")),
        }
        self.refresh_repeat_status();
        self.refresh_recurrence_preview();
    }

    pub(crate) fn canonicalize_schedule_input(&mut self) {
        if self.schedule_error.is_none() {
            self.schedule_input = LineEdit::new(crate::schedule_input::format_schedule_input(
                &self.available_at.text,
                &self.due_on.text,
                &self.repeat_rule.text,
                &self.repeat_at.text,
                &self.repeat_due,
                &self.repeat_start_on.text,
            ));
        }
    }

    pub(crate) fn apply_schedule_editor(&mut self, editor: ScheduleEditorState) {
        match editor.mode {
            ScheduleEditorMode::Once if !editor.template_locked => {
                self.available_at = editor.available_at;
                self.due_on = editor.due_on;
                self.repeat_rule = LineEdit::blank();
            }
            ScheduleEditorMode::Repeat => {
                self.available_at = LineEdit::blank();
                self.due_on = LineEdit::blank();
                self.repeat_rule = editor.repeat_rule;
                self.repeat_at = editor.repeat_at;
                self.repeat_due = editor.repeat_due;
                self.repeat_start_on = editor.repeat_start_on;
                self.time_zone = editor.time_zone;
            }
            _ => {}
        }
        self.schedule_input = LineEdit::new(crate::schedule_input::format_schedule_input(
            &self.available_at.text,
            &self.due_on.text,
            &self.repeat_rule.text,
            &self.repeat_at.text,
            &self.repeat_due,
            &self.repeat_start_on.text,
        ));
        self.schedule_error = None;
        self.refresh_repeat_status();
        self.refresh_recurrence_preview();
    }

    pub(crate) fn is_populated(&self) -> bool {
        !self.title.text.trim().is_empty()
            || self
                .description
                .buffer
                .lines
                .iter()
                .any(|line| !line.trim().is_empty())
            || self.selected_project != self.initial_project
            || self.status != AddTaskStatusChoice::Derived
            || self.priority.value() != "none"
            || !self.labels.is_empty()
            || !self.custom_metadata.is_empty()
            || self.is_epic
            || !self.available_at.text.trim().is_empty()
            || !self.due_on.text.trim().is_empty()
            || !self.attachments.is_empty()
            || self.recurrence_series_id.is_some()
            || !matches!(self.repeat_rule.text.trim(), "" | "none")
    }

    pub(crate) fn focus_next(&mut self, reverse: bool) {
        for _ in 0..AddTaskStep::ALL.len() {
            self.focus = self.focus.next(reverse);
            let has_visible_image_step =
                self.focus != AddTaskStep::Images || !self.attachments.is_empty();
            if has_visible_image_step && self.is_step_editable(self.focus) {
                break;
            }
        }
    }

    pub(crate) fn focus_metadata_next(&mut self, reverse: bool) {
        for _ in 0..AddTaskStep::ALL.len() {
            self.focus = self.focus.metadata_next(reverse);
            if self.is_step_editable(self.focus) {
                break;
            }
        }
    }

    pub(crate) fn is_step_editable(&self, step: AddTaskStep) -> bool {
        if step == AddTaskStep::Schedule {
            return true;
        }
        if step.is_schedule_field() {
            return false;
        }
        true
    }

    #[cfg(test)]
    pub(crate) fn set_repeat_rule(&mut self, repeat_rule: String) {
        self.repeat_rule = LineEdit::new(repeat_rule);
        self.refresh_repeat_status();
    }

    pub(crate) fn refresh_repeat_status(&mut self) {
        if self.recurrence_valid() {
            self.create_more = false;
        }
    }

    pub(crate) fn effective_status(&self) -> &str {
        derived_add_task_status(&self.status, &self.priority, self.recurrence_valid())
    }

    pub(crate) fn automatic_status(&self) -> &'static str {
        automatic_add_task_status(&self.priority, self.recurrence_valid())
    }

    pub(crate) fn status_is_automatic(&self) -> bool {
        matches!(self.status, AddTaskStatusChoice::Derived)
    }

    pub(crate) fn apply_status_choice(&mut self, value: &str) -> bool {
        let Some(choice) = AddTaskStatusChoice::from_picker_value(value) else {
            return false;
        };
        self.status = choice;
        true
    }

    pub(crate) fn apply_priority_choice(&mut self, value: &str) -> bool {
        let Some(choice) = AddTaskPriorityChoice::from_human_selection(value) else {
            return false;
        };
        self.priority = choice;
        true
    }

    pub(crate) fn recurrence_enabled(&self) -> bool {
        self.template_schedule.is_some() || !matches!(self.repeat_rule.text.trim(), "" | "none")
    }

    pub(crate) fn recurrence_valid(&self) -> bool {
        self.template_schedule.is_some()
            || self
                .recurrence_rule_input()
                .is_ok_and(|rule| rule.is_some())
    }

    pub(crate) fn recurrence_rule_input(&self) -> anyhow::Result<Option<String>> {
        crate::recurrence_input::canonical_rule_input(&self.repeat_rule.text)
    }

    pub(crate) fn recurrence_schedule(
        &self,
    ) -> anyhow::Result<Option<aven_core::recurrence::RecurrenceSchedule>> {
        if !self.recurrence_enabled() {
            return Ok(None);
        }
        let repeat_at = Some(self.repeat_at.text.trim()).filter(|value| !value.is_empty());
        let start_on = Some(self.repeat_start_on.text.trim()).filter(|value| !value.is_empty());
        if let Some(template) = self.template_schedule.as_ref() {
            let mutable = crate::commands::recurrence_schedule(
                "daily",
                repeat_at,
                Some(&self.repeat_due),
                Some(template.timezone.as_str()),
                Some(&template.start_on.to_string()),
            )?;
            return Ok(Some(aven_core::recurrence::RecurrenceSchedule::new(
                template.rule,
                template.timezone.clone(),
                template.start_on,
                mutable.available_local_time,
                mutable.due_policy,
            )));
        }
        let Some(rule) = self.recurrence_rule_input()? else {
            return Ok(None);
        };
        crate::commands::recurrence_schedule(
            &rule,
            repeat_at,
            Some(&self.repeat_due),
            Some(self.time_zone.trim()).filter(|value| !value.is_empty()),
            start_on,
        )
        .map(Some)
    }

    pub(crate) fn refresh_recurrence_preview(&mut self) {
        self.refresh_recurrence_preview_at(Utc::now());
    }

    pub(crate) fn refresh_recurrence_preview_at(&mut self, now: DateTime<Utc>) {
        self.recurrence_preview.clear();
        self.recurrence_error = None;
        let Some(schedule) = (match self.recurrence_schedule() {
            Ok(schedule) => schedule,
            Err(error) => {
                self.recurrence_error = Some(format!("{error:#}"));
                return;
            }
        }) else {
            return;
        };
        let zone = schedule
            .timezone
            .as_str()
            .parse::<chrono_tz::Tz>()
            .expect("core-validated time zone parses with chrono-tz");
        let from = schedule.start_on.max(now.with_timezone(&zone).date_naive());
        self.recurrence_preview = schedule
            .slots_on_or_after(from)
            .take(3)
            .map(|date| date.format("%a %b %-d %Y").to_string())
            .collect();
    }
}
