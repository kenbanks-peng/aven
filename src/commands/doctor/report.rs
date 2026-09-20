#[derive(serde::Serialize)]
pub(in crate::commands) struct DoctorReport {
    pub(in crate::commands) overall_status: DoctorStatus,
    pub(in crate::commands) sections: Vec<DoctorSection>,
}

impl DoctorReport {
    pub(super) fn new() -> Self {
        Self {
            overall_status: DoctorStatus::Ok,
            sections: Vec::new(),
        }
    }

    pub(super) fn section(
        &mut self,
        code: &'static str,
        title: &'static str,
    ) -> &mut DoctorSection {
        self.sections.push(DoctorSection {
            code,
            title,
            status: DoctorStatus::Ok,
            rows: Vec::new(),
        });
        self.sections.last_mut().expect("section was pushed")
    }

    pub(super) fn finish(&mut self) {
        for section in &mut self.sections {
            section.status = if section
                .rows
                .iter()
                .any(|row| row.status == DoctorStatus::Error)
            {
                DoctorStatus::Error
            } else if section
                .rows
                .iter()
                .any(|row| row.status == DoctorStatus::Warning)
            {
                DoctorStatus::Warning
            } else if section
                .rows
                .iter()
                .all(|row| row.status == DoctorStatus::Skipped)
            {
                DoctorStatus::Skipped
            } else {
                DoctorStatus::Ok
            };
        }
        self.overall_status = if self
            .sections
            .iter()
            .any(|section| section.status == DoctorStatus::Error)
        {
            DoctorStatus::Error
        } else if self
            .sections
            .iter()
            .any(|section| section.status == DoctorStatus::Warning)
        {
            DoctorStatus::Warning
        } else {
            DoctorStatus::Ok
        };
    }

    pub(super) fn has_errors(&self) -> bool {
        self.sections
            .iter()
            .flat_map(|section| &section.rows)
            .any(|row| row.status == DoctorStatus::Error)
    }
}

#[derive(serde::Serialize)]
pub(in crate::commands) struct DoctorSection {
    pub(in crate::commands) code: &'static str,
    pub(in crate::commands) title: &'static str,
    pub(in crate::commands) status: DoctorStatus,
    pub(in crate::commands) rows: Vec<DoctorRow>,
}

impl DoctorSection {
    pub(super) fn row(
        &mut self,
        code: impl Into<String>,
        label: &'static str,
        status: DoctorStatus,
        value: impl Into<String>,
        skipped_reason: Option<String>,
    ) {
        self.rows.push(DoctorRow {
            code: code.into(),
            status,
            label,
            value: value.into(),
            skipped_reason,
        });
    }

    pub(super) fn check(
        &mut self,
        code: impl Into<String>,
        label: &'static str,
        ok: bool,
        value: impl Into<String>,
    ) {
        self.row(
            code,
            label,
            if ok {
                DoctorStatus::Ok
            } else {
                DoctorStatus::Error
            },
            value,
            None,
        );
    }

    pub(super) fn info(
        &mut self,
        code: impl Into<String>,
        label: &'static str,
        value: impl Into<String>,
    ) {
        self.row(code, label, DoctorStatus::Info, value, None);
    }

    pub(super) fn warning(
        &mut self,
        code: impl Into<String>,
        label: &'static str,
        value: impl Into<String>,
    ) {
        self.row(code, label, DoctorStatus::Warning, value, None);
    }

    pub(super) fn skipped(
        &mut self,
        code: impl Into<String>,
        label: &'static str,
        reason: impl Into<String>,
    ) {
        let reason = reason.into();
        self.row(code, label, DoctorStatus::Skipped, "skipped", Some(reason));
    }
}

#[derive(serde::Serialize)]
pub(in crate::commands) struct DoctorRow {
    pub(in crate::commands) code: String,
    pub(in crate::commands) status: DoctorStatus,
    pub(in crate::commands) label: &'static str,
    pub(in crate::commands) value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::commands) skipped_reason: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::commands) enum DoctorStatus {
    Ok,
    Info,
    Skipped,
    Warning,
    Error,
}
