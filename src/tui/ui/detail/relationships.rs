use std::collections::BTreeSet;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::super::task_display::linked_task_ref_spans;
use super::super::task_list::EPIC_MARKER;
use super::DetailInteractiveRow;
use crate::query::TaskListItem;
use crate::tui::app::{DetailSection, DetailTargetId};
use crate::tui::text::truncate_width;
use crate::tui::theme;
use crate::tui::theme::{ACCENT, BG_PANEL, BORDER, FG, FG_DIM, FG_MUTED, YELLOW};
use crate::tui::widgets::{priority_short, status_span};

#[derive(Debug, Clone, Copy)]
pub(super) enum DependencyDirection {
    Blocker,
    Dependent,
}

impl DependencyDirection {
    pub(super) fn marker(self) -> &'static str {
        match self {
            Self::Blocker => "←",
            Self::Dependent => "→",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EpicChildState {
    Live,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DetailEpicChild {
    pub(super) link: crate::query::TaskDependencyLink,
    pub(super) dependencies: Vec<crate::query::TaskDependencyLink>,
    pub(super) state: EpicChildState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct EpicChildCounts {
    pub(super) open: usize,
    pub(super) total: usize,
}

pub(super) const DETAIL_DEPENDENCY_TREE_CAP: usize = 3;
pub(super) fn detail_epic_children(
    item: &TaskListItem,
    removed: Option<&crate::tui::app::RemovedEpicChild>,
) -> Vec<DetailEpicChild> {
    let mut children = item
        .epic_children
        .iter()
        .cloned()
        .map(|link| DetailEpicChild {
            dependencies: item
                .epic_child_dependencies
                .get(&link.task_id)
                .cloned()
                .unwrap_or_default(),
            link,
            state: EpicChildState::Live,
        })
        .collect::<Vec<_>>();
    if let Some(removed) = removed
        && removed.epic_id == item.task.id
        && !children
            .iter()
            .any(|child| child.link.task_id == removed.child.task_id)
    {
        let position = removed.original_position.min(children.len());
        children.insert(
            position,
            DetailEpicChild {
                link: removed.child.clone(),
                dependencies: Vec::new(),
                state: EpicChildState::Removed,
            },
        );
    }
    children
}

pub(super) fn push_interactive_lines(
    lines: &mut Vec<Line<'static>>,
    rows: &mut Vec<DetailInteractiveRow>,
    target: DetailTargetId,
    rendered: Vec<Line<'static>>,
) {
    let line_index = lines.len();
    let height = rendered.len();
    lines.extend(rendered);
    rows.push(DetailInteractiveRow {
        target,
        line_index,
        height,
    });
}

pub(super) fn extend_epic_parent_section(
    lines: &mut Vec<Line<'static>>,
    rows: &mut Vec<DetailInteractiveRow>,
    item: &TaskListItem,
    width: usize,
    active_target: Option<&DetailTargetId>,
) {
    let Some(parent) = &item.epic_parent else {
        return;
    };
    lines.push(Line::from(Span::styled(
        "EPIC PARENT",
        Style::new().fg(FG_DIM).add_modifier(Modifier::BOLD),
    )));
    let target = DetailTargetId::Task {
        section: DetailSection::EpicParent,
        task_id: parent.task_id.clone(),
    };
    let active = active_target == Some(&target);
    let rendered =
        epic_child_tree_item_lines(parent, &[], EpicChildState::Live, true, true, width, active);
    push_interactive_lines(lines, rows, target, rendered);
}

pub(super) fn epic_child_counts(children: &[DetailEpicChild]) -> EpicChildCounts {
    EpicChildCounts {
        open: children
            .iter()
            .filter(|child| child.state == EpicChildState::Live && child.link.unresolved)
            .count(),
        total: children
            .iter()
            .filter(|child| child.state == EpicChildState::Live)
            .count(),
    }
}

pub(super) fn ordered_epic_children(children: &[DetailEpicChild]) -> Vec<&DetailEpicChild> {
    children
        .iter()
        .filter(|child| child.link.unresolved)
        .chain(children.iter().filter(|child| !child.link.unresolved))
        .collect()
}

pub(super) fn extend_epic_children_section(
    lines: &mut Vec<Line<'static>>,
    rows: &mut Vec<DetailInteractiveRow>,
    item: &TaskListItem,
    children: &[DetailEpicChild],
    width: usize,
    active_target: Option<&DetailTargetId>,
    expanded: bool,
) {
    if !item.task.is_epic {
        return;
    }
    if !lines.is_empty() {
        lines.push(Line::from(""));
    }
    let counts = epic_child_counts(children);
    lines.push(Line::from(vec![
        Span::styled(
            "CHILD TASKS",
            Style::new().fg(FG_DIM).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" open={} total={}", counts.open, counts.total),
            Style::new().fg(FG_DIM),
        ),
    ]));
    let links = ordered_epic_children(children);
    if links.is_empty() {
        lines.push(Line::from(Span::styled("none", Style::new().fg(FG_MUTED))));
        return;
    }
    let visible = if expanded {
        links.len()
    } else {
        links.len().min(5)
    };
    let has_disclosure = links.len() > 5;
    for (index, child) in links.iter().take(visible).enumerate() {
        let target = DetailTargetId::Task {
            section: DetailSection::EpicChildren,
            task_id: child.link.task_id.clone(),
        };
        let is_last = index + 1 == visible && !has_disclosure;
        let rendered = epic_child_tree_item_lines(
            &child.link,
            &child.dependencies,
            child.state,
            is_last,
            false,
            width,
            active_target == Some(&target),
        );
        push_interactive_lines(lines, rows, target, rendered);
    }
    if has_disclosure {
        let target = DetailTargetId::Expand {
            section: DetailSection::EpicChildren,
        };
        let label = if expanded {
            "Show less".to_string()
        } else {
            format!("Show {} more", links.len() - visible)
        };
        push_disclosure_row(lines, rows, target, &label, active_target);
    }
}

pub(super) fn push_disclosure_row(
    lines: &mut Vec<Line<'static>>,
    rows: &mut Vec<DetailInteractiveRow>,
    target: DetailTargetId,
    label: &str,
    active_target: Option<&DetailTargetId>,
) {
    let active = active_target == Some(&target);
    push_interactive_lines(lines, rows, target, vec![disclosure_line(label, active)]);
}

pub(super) fn disclosure_line(label: &str, active: bool) -> Line<'static> {
    let style = if active {
        Style::new()
            .fg(ACCENT)
            .bg(BG_PANEL)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(FG_MUTED)
    };
    let tree_style = if active {
        Style::new().fg(BORDER).bg(BG_PANEL)
    } else {
        Style::new().fg(BORDER)
    };
    Line::from(vec![
        Span::styled("└─ ", tree_style),
        Span::styled(label.to_string(), style),
    ])
}

pub(super) fn epic_child_tree_item_lines(
    link: &crate::query::TaskDependencyLink,
    dependencies: &[crate::query::TaskDependencyLink],
    state: EpicChildState,
    is_last: bool,
    show_epic_marker: bool,
    width: usize,
    hovered: bool,
) -> Vec<Line<'static>> {
    let tree_glyph = if is_last { "└─ " } else { "├─ " };
    let removed = state == EpicChildState::Removed;
    let title_style = if hovered {
        Style::new().fg(FG).bg(BG_PANEL)
    } else if removed {
        Style::new().fg(FG_MUTED).add_modifier(Modifier::DIM)
    } else {
        Style::new().fg(FG)
    };
    let tree_style = if hovered {
        Style::new().fg(BORDER).bg(BG_PANEL)
    } else {
        Style::new().fg(BORDER)
    };
    let gap_style = if hovered {
        Style::new().fg(FG_DIM).bg(BG_PANEL)
    } else {
        Style::new().fg(FG_DIM)
    };
    let mut prefix = vec![Span::styled(tree_glyph, tree_style)];
    if show_epic_marker {
        let marker_style = if hovered {
            Style::new().fg(YELLOW).bg(BG_PANEL)
        } else {
            Style::new().fg(YELLOW)
        };
        prefix.extend([
            Span::styled(EPIC_MARKER, marker_style),
            Span::styled(" ", gap_style),
        ]);
    }
    let mut reference = linked_task_ref_spans(&link.display_ref, &link.project_key);
    for span in &mut reference {
        if hovered {
            span.style = span.style.bg(BG_PANEL).add_modifier(Modifier::BOLD);
        } else if removed {
            span.style = span.style.fg(FG_MUTED).add_modifier(Modifier::DIM);
        }
    }
    prefix.extend(reference);
    prefix.push(Span::styled("  ", gap_style));
    let title = if removed {
        format!("{}  [removed]", link.title)
    } else {
        link.title.clone()
    };
    let mut lines = dependency_node_lines_with_title_style(
        prefix,
        &title,
        &link.status,
        &link.priority,
        width,
        title_style,
    );
    if !removed && link.unresolved {
        lines.extend(epic_child_dependency_lines(
            dependencies,
            is_last,
            width,
            hovered,
        ));
    }
    lines
}

pub(super) fn epic_child_dependency_lines(
    dependencies: &[crate::query::TaskDependencyLink],
    child_is_last: bool,
    width: usize,
    hovered: bool,
) -> Vec<Line<'static>> {
    let blockers = dependencies
        .iter()
        .filter(|dependency| dependency.unresolved)
        .collect::<Vec<_>>();
    if blockers.is_empty() {
        return Vec::new();
    }
    let rail = if child_is_last { "   " } else { "│  " };
    let available = width.saturating_sub(rail.width());
    let mut summary = format!(
        "← {} blocker{}",
        blockers.len(),
        if blockers.len() == 1 { "" } else { "s" }
    );
    let mut visible_refs = 0;
    for visible in (1..=blockers.len().min(2)).rev() {
        let refs = blockers[..visible]
            .iter()
            .map(|link| link.display_ref.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let hidden = blockers.len() - visible;
        let suffix = if hidden == 0 {
            String::new()
        } else {
            format!(" +{hidden} more")
        };
        let candidate = format!("← blocked by {refs}{suffix}");
        if candidate.width() <= available {
            summary = candidate;
            visible_refs = visible;
            break;
        }
    }
    let rail_style = Style::new().fg(BORDER);
    let dependency_style = Style::new().fg(FG_DIM);
    let background = if hovered {
        Style::new().bg(BG_PANEL)
    } else {
        Style::new()
    };
    let mut spans = vec![Span::styled(
        truncate_width(rail, width),
        rail_style.patch(background),
    )];
    if visible_refs == 0 {
        spans.push(Span::styled(
            truncate_width(&summary, available),
            dependency_style,
        ));
    } else {
        spans.push(Span::styled("← blocked by ", dependency_style));
        for (index, link) in blockers[..visible_refs].iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(", ", dependency_style));
            }
            spans.extend(linked_task_ref_spans(&link.display_ref, &link.project_key));
        }
        let hidden = blockers.len() - visible_refs;
        if hidden > 0 {
            spans.push(Span::styled(format!(" +{hidden} more"), dependency_style));
        }
    }
    for span in &mut spans {
        span.style = span.style.patch(background);
    }
    vec![Line::from(spans)]
}

pub(super) fn extend_dependency_sections(
    lines: &mut Vec<Line<'static>>,
    rows: &mut Vec<DetailInteractiveRow>,
    item: &TaskListItem,
    width: usize,
    active_target: Option<&DetailTargetId>,
    expanded_sections: &BTreeSet<DetailSection>,
) {
    extend_dependency_section(
        lines,
        rows,
        "WHY BLOCKED",
        &item.depends_on,
        DependencyDirection::Blocker,
        DetailSection::DependsOn,
        width,
        active_target,
        expanded_sections.contains(&DetailSection::DependsOn),
    );
    extend_dependency_section(
        lines,
        rows,
        "WHAT THIS UNLOCKS",
        &item.blocks,
        DependencyDirection::Dependent,
        DetailSection::Blocks,
        width,
        active_target,
        expanded_sections.contains(&DetailSection::Blocks),
    );
}

#[allow(clippy::too_many_arguments)]
pub(super) fn extend_dependency_section(
    lines: &mut Vec<Line<'static>>,
    rows: &mut Vec<DetailInteractiveRow>,
    label: &'static str,
    links: &[crate::query::TaskDependencyLink],
    direction: DependencyDirection,
    section: DetailSection,
    width: usize,
    active_target: Option<&DetailTargetId>,
    expanded: bool,
) {
    if links.is_empty() {
        return;
    }
    lines.push(Line::from(""));
    lines.push(dependency_heading(label, links));
    let visible = if expanded {
        links.len()
    } else {
        links.len().min(DETAIL_DEPENDENCY_TREE_CAP)
    };
    let has_disclosure = links.len() > DETAIL_DEPENDENCY_TREE_CAP;
    for (index, link) in links.iter().take(visible).enumerate() {
        let target = DetailTargetId::Task {
            section,
            task_id: link.task_id.clone(),
        };
        let mut rendered = dependency_tree_item_lines(
            link,
            direction,
            index + 1 == visible && !has_disclosure,
            width,
        );
        if active_target == Some(&target) {
            apply_link_row_style(&mut rendered);
        }
        push_interactive_lines(lines, rows, target, rendered);
    }
    if has_disclosure {
        let target = DetailTargetId::Expand { section };
        let label = if expanded {
            "Show less".to_string()
        } else {
            format!("Show {} more", links.len() - visible)
        };
        push_disclosure_row(lines, rows, target, &label, active_target);
    }
}

pub(super) fn extend_related_section(
    lines: &mut Vec<Line<'static>>,
    rows: &mut Vec<DetailInteractiveRow>,
    item: &TaskListItem,
    width: usize,
    active_target: Option<&DetailTargetId>,
    expanded: bool,
) {
    let links = item
        .related
        .iter()
        .filter(|link| !link.deleted || item.task.deleted)
        .collect::<Vec<_>>();
    if links.is_empty() {
        return;
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(
            "RELATED",
            Style::new().fg(FG_DIM).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" total={}", links.len()), Style::new().fg(FG_DIM)),
    ]));
    let visible = if expanded {
        links.len()
    } else {
        links.len().min(DETAIL_DEPENDENCY_TREE_CAP)
    };
    for (index, link) in links.iter().take(visible).enumerate() {
        let target = DetailTargetId::Task {
            section: DetailSection::Related,
            task_id: link.task_id.clone(),
        };
        let glyph = if index + 1 == visible {
            "└─ "
        } else {
            "├─ "
        };
        let available = width.saturating_sub(glyph.width() + link.display_ref.width() + 4);
        let title = truncate_width(&link.title, available);
        let mut spans = vec![Span::styled(glyph, Style::new().fg(BORDER))];
        spans.extend(linked_task_ref_spans(&link.display_ref, &link.project_key));
        spans.extend([
            Span::raw("  "),
            status_span(link.status.as_str()),
            Span::raw("  "),
            Span::styled(title, Style::new().fg(FG)),
        ]);
        let mut rendered = vec![Line::from(spans)];
        if active_target == Some(&target) {
            apply_link_row_style(&mut rendered);
        }
        push_interactive_lines(lines, rows, target, rendered);
    }
    if links.len() > DETAIL_DEPENDENCY_TREE_CAP {
        let target = DetailTargetId::Expand {
            section: DetailSection::Related,
        };
        let label = if expanded {
            "Show less".to_string()
        } else {
            format!("Show {} more", links.len() - visible)
        };
        push_disclosure_row(lines, rows, target, &label, active_target);
    }
}

pub(super) fn apply_link_row_style(lines: &mut [Line<'static>]) {
    for line in lines {
        for span in &mut line.spans {
            span.style = span.style.bg(BG_PANEL);
        }
    }
}

pub(super) fn dependency_heading(
    label: &'static str,
    links: &[crate::query::TaskDependencyLink],
) -> Line<'static> {
    let open = links.iter().filter(|link| link.unresolved).count();
    Line::from(vec![
        Span::styled(label, Style::new().fg(FG_DIM).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!(" open={open} total={}", links.len()),
            Style::new().fg(FG_DIM),
        ),
    ])
}

pub(super) fn dependency_tree_item_lines(
    link: &crate::query::TaskDependencyLink,
    direction: DependencyDirection,
    is_last: bool,
    width: usize,
) -> Vec<Line<'static>> {
    let tree_glyph = if is_last { "└─ " } else { "├─ " };
    let mut prefix = vec![
        Span::styled(tree_glyph, Style::new().fg(BORDER)),
        Span::styled(direction.marker(), Style::new().fg(FG_DIM)),
        Span::styled(" ", Style::new().fg(FG_DIM)),
    ];
    prefix.extend(linked_task_ref_spans(&link.display_ref, &link.project_key));
    prefix.push(Span::styled("  ", Style::new().fg(FG_DIM)));
    dependency_node_lines(prefix, &link.title, &link.status, &link.priority, width)
}

pub(super) fn dependency_node_lines(
    spans: Vec<Span<'static>>,
    title: &str,
    status: &str,
    priority: &str,
    width: usize,
) -> Vec<Line<'static>> {
    dependency_node_lines_with_title_style(
        spans,
        title,
        status,
        priority,
        width,
        Style::new().fg(FG),
    )
}

pub(super) fn dependency_node_lines_with_title_style(
    mut spans: Vec<Span<'static>>,
    title: &str,
    status: &str,
    priority: &str,
    width: usize,
    title_style: Style,
) -> Vec<Line<'static>> {
    let title_width = dependency_title_width(&spans, status, priority, width);
    if title_width > 0 {
        spans.push(Span::styled(
            truncate_width(title, title_width),
            title_style,
        ));
        spans.push(Span::styled("  ", Style::new().fg(FG_DIM)));
        spans.push(status_span(status));
        spans.push(Span::styled("  ", Style::new().fg(FG_DIM)));
        spans.push(Span::styled(
            priority_short(priority),
            theme::priority_style(priority).add_modifier(Modifier::BOLD),
        ));
        return vec![Line::from(spans)];
    }

    let continuation_prefix = dependency_continuation_prefix(&spans);
    let mut lines = vec![Line::from(spans)];
    lines.push(Line::from(vec![
        Span::styled(continuation_prefix.clone(), Style::new().fg(BORDER)),
        Span::styled(
            truncate_width(title, width.saturating_sub(continuation_prefix.width())),
            title_style,
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled(continuation_prefix.clone(), Style::new().fg(BORDER)),
        status_span(status),
        Span::styled("  ", Style::new().fg(FG_DIM)),
        Span::styled(
            priority_short(priority),
            theme::priority_style(priority).add_modifier(Modifier::BOLD),
        ),
    ]));
    lines
}

fn dependency_continuation_prefix(prefix: &[Span<'static>]) -> String {
    " ".repeat(prefix.iter().map(Span::width).sum::<usize>().min(4))
}

fn dependency_title_width(
    prefix: &[Span<'static>],
    status: &str,
    priority: &str,
    width: usize,
) -> usize {
    let prefix_width: usize = prefix.iter().map(Span::width).sum();
    let trailing_width = 4 + status_span(status).width() + priority_short(priority).width();
    width.saturating_sub(prefix_width + trailing_width)
}
