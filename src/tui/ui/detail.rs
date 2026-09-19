use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Clear, Padding, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use std::collections::{BTreeSet, HashSet};
use std::rc::Rc;

use super::input::clipped_input_line;
use super::scroll::{clamp_scroll_start, scrollbar_thumb_position};
use super::task_display::{description_or_placeholder, labels_display, linked_task_ref_spans};
use super::task_list::EPIC_MARKER;
use super::timestamps::{local_activity_timestamp_display, local_timestamp_display};
use super::truncate::truncate_line_width;
use crate::query::TaskListItem;
use crate::task_render::{AttachmentMetadataJson, attachment_state_placeholder, human_file_size};
use crate::tui::app::{DetailSection, DetailTargetId, WidgetState};
use crate::tui::detail_selection::{DetailTextSelection, TextCell, text_cell_at_column};
use crate::tui::markdown::{
    MarkdownBlock, MarkdownRenderContext, render_markdown_with_context_without_link_urls,
    render_markdown_without_link_urls,
};
use crate::tui::overlay::TextInputView;
use crate::tui::store::{DetailRevision, TuiStore};
use crate::tui::text::truncate_width;
use crate::tui::theme::{
    self, ACCENT, BG, BG_PANEL, BORDER, FG, FG_DIM, FG_MUTED, INVERSE_FG, ORANGE, RED, YELLOW,
};
use crate::tui::widgets::{priority_short, status_chip, status_span};
use unicode_width::UnicodeWidthStr;

const DETAIL_DEPENDENCY_TREE_CAP: usize = 3;

pub(crate) fn detail_target_is_actionable(item: &TaskListItem, target: &DetailTargetId) -> bool {
    match target {
        DetailTargetId::CustomMetadata => !item.metadata.is_empty(),
        DetailTargetId::Task { section, task_id } => match section {
            DetailSection::EpicParent => item
                .epic_parent
                .as_ref()
                .is_some_and(|link| &link.task_id == task_id),
            DetailSection::EpicChildren => item
                .epic_children
                .iter()
                .any(|link| &link.task_id == task_id),
            DetailSection::DependsOn => item.depends_on.iter().any(|link| &link.task_id == task_id),
            DetailSection::Blocks => item.blocks.iter().any(|link| &link.task_id == task_id),
            DetailSection::Related => item
                .related
                .iter()
                .any(|link| (!link.deleted || item.task.deleted) && &link.task_id == task_id),
            DetailSection::CustomMetadata
            | DetailSection::Attachments
            | DetailSection::Notes
            | DetailSection::Activity => false,
        },
        DetailTargetId::Note { note_id } => item.notes.iter().any(|note| note.id == *note_id),
        DetailTargetId::Attachment { attachment_id } => item
            .attachments
            .iter()
            .find(|attachment| attachment.attachment_id == *attachment_id)
            .is_some_and(attachment_is_locally_openable),
        DetailTargetId::Expand { section } => match section {
            DetailSection::EpicChildren => item.epic_children.len() > 5,
            DetailSection::DependsOn => item.depends_on.len() > DETAIL_DEPENDENCY_TREE_CAP,
            DetailSection::Blocks => item.blocks.len() > DETAIL_DEPENDENCY_TREE_CAP,
            DetailSection::Related => {
                item.related
                    .iter()
                    .filter(|link| !link.deleted || item.task.deleted)
                    .count()
                    > DETAIL_DEPENDENCY_TREE_CAP
            }
            DetailSection::Activity => !item.activity.is_empty(),
            DetailSection::CustomMetadata
            | DetailSection::EpicParent
            | DetailSection::Attachments
            | DetailSection::Notes => false,
        },
    }
}

#[derive(Debug, Clone, Copy)]
enum DependencyDirection {
    Blocker,
    Dependent,
}

impl DependencyDirection {
    fn marker(self) -> &'static str {
        match self {
            Self::Blocker => "←",
            Self::Dependent => "→",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DetailContentLayout {
    body_area: Rect,
    content_area: Rect,
    metadata_area: Rect,
}

#[derive(Clone, Copy)]
pub(crate) struct DetailRenderContext<'a> {
    pub(crate) terminal_area: Rect,
    pub(crate) scroll: u16,
    pub(crate) detail_revision: DetailRevision,
    pub(crate) inline_title_editor: Option<&'a TextInputView>,
    pub(crate) active_target: Option<&'a DetailTargetId>,
    pub(crate) hovered_target: Option<&'a DetailTargetId>,
    pub(crate) expanded_sections: &'a BTreeSet<DetailSection>,
    pub(crate) selection: Option<&'a DetailTextSelection>,
    pub(crate) inline_images: Option<&'a DetailInlineImageContext>,
    pub(crate) pending_attachments:
        &'a [crate::tui::attachment_controller::PendingAttachmentView],
    pub(crate) removed_epic_child: Option<&'a crate::tui::app::RemovedEpicChild>,
}

impl DetailRenderContext<'_> {
    fn content_layout(&self) -> DetailContentLayout {
        detail_content_layout(self.terminal_area)
    }
}

#[derive(Debug, Clone)]
struct DetailContentRenderModel {
    sticky_lines: Vec<Line<'static>>,
    lines: Vec<Line<'static>>,
    content_height: usize,
    body_start: usize,
    scrollbar_position: usize,
    image_placements: Rc<Vec<DetailBodyImagePlacement>>,
    interactive_rows: Rc<Vec<DetailInteractiveRow>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DetailInteractiveRow {
    pub(crate) target: DetailTargetId,
    pub(crate) line_index: usize,
    pub(crate) height: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DetailInlineImageContext {
    pub(crate) previews_enabled: bool,
    pub(crate) unavailable_hashes: HashSet<String>,
    pub(crate) focused_attachment_id: Option<String>,
}

impl Default for DetailInlineImageContext {
    fn default() -> Self {
        Self {
            previews_enabled: true,
            unavailable_hashes: HashSet::new(),
            focused_attachment_id: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DetailInlineImagePlacement {
    pub(crate) attachment_id: String,
    pub(crate) source_hash: String,
    pub(crate) x: u16,
    pub(crate) y: u16,
    pub(crate) width: u16,
    pub(crate) height: u16,
}

#[derive(Debug, Clone)]
struct DetailBodyImagePlacement {
    attachment_id: String,
    source_hash: String,
    line_index: usize,
    width: u16,
    height: u16,
}

#[derive(Debug, Clone)]
struct DetailBodyAttachmentPlacement {
    attachment_id: String,
    line_index: usize,
    height: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EpicChildState {
    Live,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DetailEpicChild {
    link: crate::query::TaskDependencyLink,
    dependencies: Vec<crate::query::TaskDependencyLink>,
    state: EpicChildState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EpicChildCounts {
    open: usize,
    total: usize,
}

#[derive(Debug, Clone)]
struct DetailBodyDocument {
    lines: Vec<Line<'static>>,
    image_placements: Rc<Vec<DetailBodyImagePlacement>>,
    interactive_rows: Rc<Vec<DetailInteractiveRow>>,
    hyperlinks: Vec<DetailHyperlink>,
    selectable_description: Vec<SelectableLine>,
    selectable_text: String,
    section_body_indices: Vec<usize>,
}

#[derive(Debug, Clone)]
enum DetailBodyBlock {
    Line(Line<'static>),
    Image {
        placeholder: Line<'static>,
        attachment_id: String,
        source_hash: String,
        width: u16,
        height: u16,
    },
}

#[derive(Debug, Clone)]
struct SelectableLine {
    text: String,
    document_start: usize,
    body_index: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DetailHyperlink {
    url: String,
    line_index: usize,
    start_column: usize,
    end_column: usize,
}

#[derive(Debug, Clone)]
struct DetailSelectableDocument {
    text: String,
    title: Vec<SelectableLine>,
    description: Vec<SelectableLine>,
}

#[derive(Debug)]
struct DetailBodyGeometry {
    task_id: crate::ids::TaskId,
    detail_revision: DetailRevision,
    content_width: usize,
    expanded_sections: BTreeSet<DetailSection>,
    inline_images: Option<DetailInlineImageContext>,
    pending_attachments: Vec<crate::tui::attachment_controller::PendingAttachmentView>,
    removed_epic_child: Option<crate::tui::app::RemovedEpicChild>,
    epic_children: Vec<DetailEpicChild>,
    body: DetailBodyDocument,
    selectable: DetailSelectableDocument,
}

#[derive(Debug)]
pub(crate) struct DetailDocument {
    geometry: Rc<DetailBodyGeometry>,
    layout: DetailContentLayout,
    scroll: u16,
    inline_title_editor: Option<(String, usize)>,
    model: DetailContentRenderModel,
    #[cfg(test)]
    projection_id: usize,
}

#[cfg(test)]
pub(crate) struct DetailChildHit {
    pub(crate) task_id: crate::ids::TaskId,
}

#[cfg(test)]
pub(crate) struct DetailAttachmentHit {
    pub(crate) attachment_id: String,
}

pub(crate) struct DetailCopyHit {
    pub(crate) value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DetailMetadataTarget {
    Project,
    Status,
    Priority,
    Labels,
    Availability,
    Due,
}

fn detail_epic_children(
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

impl DetailBodyGeometry {
    fn build(item: &TaskListItem, context: &DetailRenderContext<'_>) -> Self {
        let content_width = context.content_layout().content_area.width as usize;
        let inline_images = context.inline_images.cloned().map(|mut images| {
            images.focused_attachment_id = None;
            images
        });
        let epic_children = detail_epic_children(item, context.removed_epic_child);
        let body = build_detail_body_document(
            item,
            &epic_children,
            content_width,
            context.expanded_sections,
            inline_images.as_ref(),
            context.pending_attachments,
        );
        let selectable = detail_selectable_document_from_body(item, content_width, true, &body);
        Self {
            task_id: item.task.id.clone(),
            detail_revision: context.detail_revision,
            content_width,
            expanded_sections: context.expanded_sections.clone(),
            inline_images,
            pending_attachments: context.pending_attachments.to_vec(),
            removed_epic_child: context.removed_epic_child.cloned(),
            epic_children,
            body,
            selectable,
        }
    }
}

impl DetailDocument {
    pub(crate) fn build(item: &TaskListItem, context: &DetailRenderContext<'_>) -> Self {
        let geometry = Rc::new(DetailBodyGeometry::build(item, context));
        Self::from_geometry(geometry, item, context)
    }

    fn from_geometry(
        geometry: Rc<DetailBodyGeometry>,
        item: &TaskListItem,
        context: &DetailRenderContext<'_>,
    ) -> Self {
        let layout = context.content_layout();
        let sticky_lines = detail_header_options(
            item,
            layout.content_area.width as usize,
            context.inline_title_editor,
        );
        let model = project_detail_content_model(
            sticky_lines,
            &geometry.body,
            layout.content_area.height as usize,
            context.scroll,
        );
        Self {
            geometry,
            layout,
            scroll: context.scroll,
            inline_title_editor: context
                .inline_title_editor
                .map(|editor| (editor.input.to_string(), editor.cursor)),
            model,
            #[cfg(test)]
            projection_id: next_detail_projection_id(),
        }
    }

    fn reproject(&self, item: &TaskListItem, context: &DetailRenderContext<'_>) -> Self {
        Self::from_geometry(Rc::clone(&self.geometry), item, context)
    }

    pub(crate) fn reuse_or_build(
        cached: Option<&Rc<Self>>,
        item: &TaskListItem,
        context: &DetailRenderContext<'_>,
    ) -> Rc<Self> {
        let Some(cached) = cached else {
            return Rc::new(Self::build(item, context));
        };
        if !cached.geometry_matches(item, context) {
            return Rc::new(Self::build(item, context));
        }
        if cached.view_matches(context) {
            Rc::clone(cached)
        } else {
            Rc::new(cached.reproject(item, context))
        }
    }

    fn geometry_matches(&self, item: &TaskListItem, context: &DetailRenderContext<'_>) -> bool {
        let layout = context.content_layout();
        let geometry = &self.geometry;
        geometry.task_id == item.task.id
            && geometry.detail_revision == context.detail_revision
            && geometry.content_width == layout.content_area.width as usize
            && geometry.expanded_sections == *context.expanded_sections
            && detail_inline_image_geometry_matches(
                geometry.inline_images.as_ref(),
                context.inline_images,
            )
            && geometry.pending_attachments == context.pending_attachments
            && geometry.removed_epic_child.as_ref() == context.removed_epic_child
    }

    fn view_matches(&self, context: &DetailRenderContext<'_>) -> bool {
        self.layout == context.content_layout()
            && self.scroll == context.scroll
            && self.inline_title_editor.as_ref()
                == context
                    .inline_title_editor
                    .map(|editor| (&editor.input, editor.cursor))
                    .map(|(input, cursor)| (input.clone(), cursor))
                    .as_ref()
    }

    fn render(
        &self,
        frame: &mut Frame,
        item: &TaskListItem,
        context: &DetailRenderContext<'_>,
        widgets: &mut WidgetState,
    ) {
        frame.render_widget(Clear, self.layout.body_area);
        frame.render_widget(
            Block::new().style(Style::new().bg(BG)),
            self.layout.body_area,
        );
        if self.layout.body_area.width == 0 || self.layout.body_area.height == 0 {
            return;
        }
        let mut model = self.model.clone();
        if let Some(active_target) = context.active_target {
            apply_active_style(&mut model, active_target);
        }
        if context.hovered_target != context.active_target
            && let Some(hovered_target) = context.hovered_target
        {
            apply_hover_style(&mut model, hovered_target);
        }
        if context.inline_title_editor.is_none()
            && let Some(selection) = context.selection.filter(|selection| {
                selection.task_id == item.task.id
                    && selection.terminal_width == context.terminal_area.width
            })
        {
            apply_detail_selection_from_document(
                &self.geometry.selectable,
                selection,
                &mut model.sticky_lines,
                &mut model.lines,
                model.body_start,
            );
        }
        render_detail_content_from_model(frame, self.layout.content_area, model, widgets);
        if self.layout.metadata_area.width > 0 {
            render_detail_metadata(
                frame,
                item,
                &self.geometry.epic_children,
                self.layout.metadata_area,
            );
        }
    }

    #[cfg(test)]
    pub(crate) fn matches_frame(
        &self,
        item: &TaskListItem,
        context: &DetailRenderContext<'_>,
    ) -> bool {
        self.geometry_matches(item, context) && self.view_matches(context)
    }

    fn sticky_height(&self) -> usize {
        self.model
            .sticky_lines
            .len()
            .min(self.layout.content_area.height as usize)
    }

    fn body_visible(&self) -> usize {
        (self.layout.content_area.height as usize).saturating_sub(self.sticky_height())
    }

    pub(crate) fn scroll_cap(&self) -> u16 {
        self.model
            .content_height
            .saturating_sub(self.body_visible()) as u16
    }

    #[cfg(test)]
    pub(crate) fn interactive_rows(&self) -> &[DetailInteractiveRow] {
        self.geometry.body.interactive_rows.as_slice()
    }

    pub(crate) fn focus_targets(&self, item: &TaskListItem) -> Vec<DetailTargetId> {
        self.geometry
            .body
            .interactive_rows
            .iter()
            .map(|row| &row.target)
            .filter(|target| match target {
                DetailTargetId::Task {
                    section: DetailSection::EpicChildren,
                    task_id,
                } => self
                    .geometry
                    .epic_children
                    .iter()
                    .any(|child| &child.link.task_id == task_id),
                DetailTargetId::Expand {
                    section: DetailSection::EpicChildren,
                } => self.geometry.epic_children.len() > 5,
                _ => detail_target_is_actionable(item, target),
            })
            .cloned()
            .collect()
    }

    pub(crate) fn link_at_position(&self, column: u16, row: u16) -> Option<String> {
        let body_y = self
            .layout
            .content_area
            .y
            .saturating_add(self.sticky_height() as u16);
        if row < body_y
            || row
                >= self
                    .layout
                    .content_area
                    .y
                    .saturating_add(self.layout.content_area.height)
            || column < self.layout.content_area.x
        {
            return None;
        }
        let line_index = self
            .model
            .body_start
            .saturating_add(row.saturating_sub(body_y) as usize);
        let local_column = column.saturating_sub(self.layout.content_area.x) as usize;
        self.geometry
            .body
            .hyperlinks
            .iter()
            .find(|link| {
                link.line_index == line_index
                    && (link.start_column..link.end_column).contains(&local_column)
            })
            .map(|link| link.url.clone())
    }

    pub(crate) fn target_at_position(&self, column: u16, row: u16) -> Option<DetailTargetId> {
        if column < self.layout.content_area.x
            || column
                >= self
                    .layout
                    .content_area
                    .x
                    .saturating_add(self.layout.content_area.width)
        {
            return None;
        }
        let body_y = self
            .layout
            .content_area
            .y
            .saturating_add(self.sticky_height() as u16);
        if row < body_y
            || row
                >= self
                    .layout
                    .content_area
                    .y
                    .saturating_add(self.layout.content_area.height)
        {
            return None;
        }
        let body_index = self
            .model
            .body_start
            .saturating_add(row.saturating_sub(body_y) as usize);
        self.geometry
            .body
            .interactive_rows
            .iter()
            .find(|target| {
                (target.line_index..target.line_index.saturating_add(target.height))
                    .contains(&body_index)
            })
            .map(|target| target.target.clone())
    }

    #[cfg(test)]
    pub(crate) fn attachment_at_position(
        &self,
        item: &TaskListItem,
        column: u16,
        row: u16,
    ) -> Option<String> {
        let DetailTargetId::Attachment { attachment_id } = self.target_at_position(column, row)?
        else {
            return None;
        };
        item.attachments
            .iter()
            .find(|attachment| attachment.attachment_id == attachment_id)
            .filter(|attachment| attachment_is_locally_openable(attachment))
            .map(|_| attachment_id)
    }

    #[cfg(test)]
    pub(crate) fn child_task_at_position(
        &self,
        column: u16,
        row: u16,
    ) -> Option<crate::ids::TaskId> {
        match self.target_at_position(column, row)? {
            DetailTargetId::Task {
                section: DetailSection::EpicChildren,
                task_id,
            } => Some(task_id),
            _ => None,
        }
    }

    pub(crate) fn target_scroll_target(&self, target: &DetailTargetId, scroll: u16) -> Option<u16> {
        let visible = self.body_visible();
        let row = self
            .geometry
            .body
            .interactive_rows
            .iter()
            .find(|row| &row.target == target)?;
        if visible == 0 {
            return None;
        }
        let cap = self.model.content_height.saturating_sub(visible);
        let scroll = (scroll as usize).min(cap);
        let end = row.line_index.saturating_add(row.height.saturating_sub(1));
        let target_scroll = if row.line_index < scroll {
            row.line_index
        } else if end >= scroll.saturating_add(visible) {
            end.saturating_add(1).saturating_sub(visible)
        } else {
            scroll
        };
        Some(target_scroll.min(cap) as u16)
    }

    pub(crate) fn section_scroll_target(&self, reverse: bool) -> u16 {
        let scroll_cap = self
            .model
            .content_height
            .saturating_sub(self.body_visible());
        let mut targets = self
            .geometry
            .body
            .section_body_indices
            .iter()
            .map(|index| (*index).min(scroll_cap) as u16)
            .collect::<Vec<_>>();
        targets.dedup();
        if reverse {
            targets
                .iter()
                .rev()
                .find(|&&target| target < self.model.body_start as u16)
                .copied()
                .or_else(|| targets.last().copied())
                .unwrap_or(0)
        } else {
            targets
                .iter()
                .find(|&&target| target > self.model.body_start as u16)
                .copied()
                .or_else(|| targets.first().copied())
                .unwrap_or(0)
        }
    }

    pub(crate) fn text_cell_at_position(&self, column: u16, row: u16) -> Option<TextCell> {
        if column < self.layout.body_area.x
            || column
                >= self
                    .layout
                    .content_area
                    .x
                    .saturating_add(self.layout.content_area.width)
            || row < self.layout.content_area.y
            || row
                >= self
                    .layout
                    .content_area
                    .y
                    .saturating_add(self.layout.content_area.height)
        {
            return None;
        }
        let title_row = row.saturating_sub(self.layout.content_area.y) as usize;
        let selectable = if let Some(title) = self.geometry.selectable.title.get(title_row) {
            title
        } else {
            let body_y = self
                .layout
                .content_area
                .y
                .saturating_add(self.sticky_height() as u16);
            if row < body_y || row >= body_y.saturating_add(self.layout.content_area.height) {
                return None;
            }
            let body_index = self
                .model
                .body_start
                .saturating_add(row.saturating_sub(body_y) as usize);
            self.geometry
                .selectable
                .description
                .iter()
                .find(|line| line.body_index == Some(body_index))?
        };
        let text_x = self
            .layout
            .content_area
            .x
            .saturating_add(u16::from(selectable.body_index.is_some()) * 2);
        let cell_column = column.saturating_sub(text_x) as usize;
        let local = text_cell_at_column(&selectable.text, cell_column).or_else(|| {
            let edge_column = if column <= text_x {
                0
            } else {
                selectable.text.width().checked_sub(1)?
            };
            text_cell_at_column(&selectable.text, edge_column)
        })?;
        Some(TextCell {
            start: selectable.document_start + local.start,
            end: selectable.document_start + local.end,
        })
    }

    pub(crate) fn selected_text(&self, selection: &DetailTextSelection) -> Option<String> {
        if selection.task_id != self.geometry.task_id {
            return None;
        }
        self.geometry
            .selectable
            .text
            .get(selection.range())
            .map(str::to_string)
    }

    #[cfg(test)]
    pub(crate) fn projection_id(&self) -> usize {
        self.projection_id
    }
}

#[cfg(test)]
fn next_detail_projection_id() -> usize {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_ID: AtomicUsize = AtomicUsize::new(1);
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

fn render_detail(
    frame: &mut Frame,
    item: &TaskListItem,
    context: &DetailRenderContext<'_>,
    widgets: &mut WidgetState,
) {
    let document = DetailDocument::reuse_or_build(widgets.detail_document.as_ref(), item, context);
    document.render(frame, item, context, widgets);
    widgets.detail_document = Some(document);
}

fn detail_query_context<'a>(
    terminal_width: u16,
    terminal_height: u16,
    scroll: u16,
    expanded_sections: &'a BTreeSet<DetailSection>,
    inline_images: Option<&'a DetailInlineImageContext>,
) -> DetailRenderContext<'a> {
    DetailRenderContext {
        terminal_area: Rect::new(0, 0, terminal_width, terminal_height),
        scroll,
        detail_revision: DetailRevision::UNCACHED,
        inline_title_editor: None,
        active_target: None,
        hovered_target: None,
        expanded_sections,
        selection: None,
        inline_images,
        pending_attachments: &[],
        removed_epic_child: None,
    }
}

fn detail_content_layout(frame_area: Rect) -> DetailContentLayout {
    let body = detail_body_area(frame_area);

    let [content_area, metadata_area] = if body.width >= 96 {
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(34)]).areas(body)
    } else {
        [body, Rect::default()]
    };
    let content_area = content_area.inner(detail_content_margin());
    DetailContentLayout {
        body_area: body,
        content_area,
        metadata_area,
    }
}

fn detail_body_area(frame_area: Rect) -> Rect {
    let [_, body, _] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Fill(1),
        Constraint::Length(2),
    ])
    .areas(frame_area);
    body
}

fn keycap_style() -> Style {
    Style::new()
        .fg(FG)
        .bg(BG_PANEL)
        .add_modifier(Modifier::BOLD)
}

#[cfg(test)]
pub(crate) fn detail_scroll_cap(
    item: &TaskListItem,
    terminal_width: u16,
    terminal_height: u16,
) -> u16 {
    detail_scroll_cap_with_images(item, terminal_width, terminal_height, None)
}

pub(crate) fn detail_scroll_cap_with_images(
    item: &TaskListItem,
    terminal_width: u16,
    terminal_height: u16,
    inline_images: Option<&DetailInlineImageContext>,
) -> u16 {
    let expanded_sections = BTreeSet::new();
    DetailDocument::build(
        item,
        &detail_query_context(
            terminal_width,
            terminal_height,
            0,
            &expanded_sections,
            inline_images,
        ),
    )
    .scroll_cap()
}

#[cfg(test)]
pub(crate) fn detail_section_scroll_target(
    item: &TaskListItem,
    scroll: u16,
    terminal_width: u16,
    terminal_height: u16,
    reverse: bool,
) -> u16 {
    detail_section_scroll_target_with_images(
        item,
        scroll,
        terminal_width,
        terminal_height,
        reverse,
        None,
    )
}

#[cfg(test)]
pub(crate) fn detail_interactive_rows(
    item: &TaskListItem,
    terminal_width: u16,
    terminal_height: u16,
    inline_images: Option<&DetailInlineImageContext>,
    expanded_sections: &BTreeSet<DetailSection>,
) -> Vec<DetailInteractiveRow> {
    DetailDocument::build(
        item,
        &detail_query_context(
            terminal_width,
            terminal_height,
            0,
            expanded_sections,
            inline_images,
        ),
    )
    .interactive_rows()
    .to_vec()
}

#[cfg(test)]
pub(crate) fn detail_attachment_scroll_target(
    item: &TaskListItem,
    attachment_id: &str,
    scroll: u16,
    terminal_width: u16,
    terminal_height: u16,
    inline_images: &DetailInlineImageContext,
) -> Option<u16> {
    let expanded_sections = BTreeSet::new();
    let document = DetailDocument::build(
        item,
        &detail_query_context(
            terminal_width,
            terminal_height,
            scroll,
            &expanded_sections,
            Some(inline_images),
        ),
    );
    document.target_scroll_target(
        &DetailTargetId::Attachment {
            attachment_id: attachment_id.to_string(),
        },
        scroll,
    )
}

#[cfg(test)]
pub(crate) fn detail_section_scroll_target_with_images(
    item: &TaskListItem,
    scroll: u16,
    terminal_width: u16,
    terminal_height: u16,
    reverse: bool,
    inline_images: Option<&DetailInlineImageContext>,
) -> u16 {
    let expanded_sections = BTreeSet::new();
    DetailDocument::build(
        item,
        &detail_query_context(
            terminal_width,
            terminal_height,
            scroll,
            &expanded_sections,
            inline_images,
        ),
    )
    .section_scroll_target(reverse)
}

fn detail_content_margin() -> Margin {
    Margin {
        horizontal: 2,
        vertical: 1,
    }
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn build_detail_content_model(
    item: &TaskListItem,
    area: Rect,
    scroll: u16,
    inline_title_editor: Option<&TextInputView>,
    active_target: Option<&DetailTargetId>,
    expanded_sections: &BTreeSet<DetailSection>,
    selection: Option<&DetailTextSelection>,
    inline_images: Option<&DetailInlineImageContext>,
) -> DetailContentRenderModel {
    build_detail_content_model_with_pending(
        item,
        area,
        scroll,
        inline_title_editor,
        active_target,
        expanded_sections,
        selection,
        inline_images,
        &[],
    )
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn build_detail_content_model_with_pending(
    item: &TaskListItem,
    area: Rect,
    scroll: u16,
    inline_title_editor: Option<&TextInputView>,
    active_target: Option<&DetailTargetId>,
    expanded_sections: &BTreeSet<DetailSection>,
    selection: Option<&DetailTextSelection>,
    inline_images: Option<&DetailInlineImageContext>,
    pending_attachments: &[crate::tui::attachment_controller::PendingAttachmentView],
) -> DetailContentRenderModel {
    let epic_children = detail_epic_children(item, None);
    let body = build_detail_body_document(
        item,
        &epic_children,
        area.width as usize,
        expanded_sections,
        inline_images,
        pending_attachments,
    );
    let mut model = project_detail_content_model(
        detail_header_options(item, area.width as usize, inline_title_editor),
        &body,
        area.height as usize,
        scroll,
    );
    if let Some(active_target) = active_target {
        apply_active_style(&mut model, active_target);
    }
    if inline_title_editor.is_none()
        && let Some(selection) = selection.filter(|selection| selection.task_id == item.task.id)
    {
        let selectable =
            detail_selectable_document_from_body(item, area.width as usize, true, &body);
        apply_detail_selection_from_document(
            &selectable,
            selection,
            &mut model.sticky_lines,
            &mut model.lines,
            model.body_start,
        );
    }
    model
}

fn project_detail_content_model(
    sticky_lines: Vec<Line<'static>>,
    body: &DetailBodyDocument,
    area_height: usize,
    scroll: u16,
) -> DetailContentRenderModel {
    let content_height = body.lines.len().max(1);
    let sticky_height = sticky_lines.len().min(area_height);
    let visible = area_height.saturating_sub(sticky_height);
    let start = clamp_scroll_start(scroll, content_height, visible.max(1));
    let end = start.saturating_add(visible).min(body.lines.len());
    let lines = body.lines[start.min(body.lines.len())..end].to_vec();
    let scrollbar_position = if content_height > visible {
        scrollbar_thumb_position(start, content_height, visible.max(1))
    } else {
        0
    };
    DetailContentRenderModel {
        sticky_lines,
        lines,
        content_height,
        body_start: start,
        scrollbar_position,
        image_placements: Rc::clone(&body.image_placements),
        interactive_rows: Rc::clone(&body.interactive_rows),
    }
}

fn detail_inline_image_geometry_matches(
    cached: Option<&DetailInlineImageContext>,
    current: Option<&DetailInlineImageContext>,
) -> bool {
    match (cached, current) {
        (None, None) => true,
        (Some(cached), Some(current)) => {
            cached.previews_enabled == current.previews_enabled
                && cached.unavailable_hashes == current.unavailable_hashes
        }
        _ => false,
    }
}

fn interactive_row_lines_mut<'a>(
    model: &'a mut DetailContentRenderModel,
    target: &DetailTargetId,
) -> Option<&'a mut [Line<'static>]> {
    let row = model
        .interactive_rows
        .iter()
        .find(|row| &row.target == target)?;
    let visible_start = model.body_start;
    let visible_end = visible_start.saturating_add(model.lines.len());
    let row_start = row.line_index.max(visible_start);
    let row_end = row.line_index.saturating_add(row.height).min(visible_end);
    if row_start >= row_end {
        return None;
    }
    Some(&mut model.lines[row_start - visible_start..row_end - visible_start])
}

fn apply_active_style(model: &mut DetailContentRenderModel, target: &DetailTargetId) {
    let Some(lines) = interactive_row_lines_mut(model, target) else {
        return;
    };
    match target {
        DetailTargetId::CustomMetadata | DetailTargetId::Expand { .. } => {
            for line in lines {
                for (index, span) in line.spans.iter_mut().enumerate() {
                    span.style = span.style.bg(BG_PANEL);
                    if index == 0 {
                        span.style = span.style.fg(BORDER);
                    } else {
                        span.style = span.style.fg(ACCENT).add_modifier(Modifier::BOLD);
                    }
                }
            }
        }
        DetailTargetId::Note { .. } => {
            for line in lines {
                for span in &mut line.spans {
                    span.style = span.style.bg(BG_PANEL);
                }
            }
        }
        DetailTargetId::Attachment { .. } => {
            for line in lines {
                for span in line.spans.iter_mut().skip(1) {
                    span.style = span.style.fg(ACCENT);
                }
            }
        }
        DetailTargetId::Task {
            section: DetailSection::EpicParent | DetailSection::EpicChildren,
            ..
        } => {
            for line in lines {
                for (index, span) in line.spans.iter_mut().enumerate() {
                    span.style = span.style.bg(BG_PANEL);
                    match index {
                        0 => span.style = span.style.fg(BORDER),
                        1 => {
                            span.style = span.style.fg(ACCENT).add_modifier(Modifier::BOLD);
                        }
                        2 => span.style = span.style.fg(FG_DIM),
                        _ => {}
                    }
                }
            }
        }
        DetailTargetId::Task { .. } => apply_link_row_style(lines),
    }
}

fn apply_hover_style(model: &mut DetailContentRenderModel, target: &DetailTargetId) {
    let Some(lines) = interactive_row_lines_mut(model, target) else {
        return;
    };
    for line in lines {
        for span in &mut line.spans {
            if matches!(target, DetailTargetId::Note { .. }) {
                span.style = span.style.bg(BG_PANEL);
            } else {
                span.style = span.style.add_modifier(Modifier::UNDERLINED);
            }
        }
    }
}

fn visible_detail_image_rect(
    body_area: Rect,
    model: &DetailContentRenderModel,
    placement: &DetailBodyImagePlacement,
) -> Option<Rect> {
    let row = placement.line_index.checked_sub(model.body_start)?;
    let frame_start = row.checked_sub(1)?;
    let frame_end = row.saturating_add(placement.height as usize);
    if frame_end >= body_area.height as usize || frame_start >= body_area.height as usize {
        return None;
    }
    let width = placement.width.min(body_area.width.saturating_sub(4));
    if placement.height == 0 || width == 0 {
        return None;
    }
    Some(Rect::new(
        body_area.x.saturating_add(3),
        body_area.y.saturating_add(row as u16),
        width,
        placement.height,
    ))
}

fn render_detail_content_from_model(
    frame: &mut Frame,
    area: Rect,
    model: DetailContentRenderModel,
    widgets: &mut WidgetState,
) {
    let visible = area.height as usize;
    let sticky_height = model.sticky_lines.len().min(visible);
    let [sticky_area, body_area] = Layout::vertical([
        Constraint::Length(sticky_height as u16),
        Constraint::Fill(1),
    ])
    .areas(area);
    let images = model
        .image_placements
        .iter()
        .filter_map(|placement| {
            let image = visible_detail_image_rect(body_area, &model, placement)?;
            Some(DetailInlineImagePlacement {
                attachment_id: placement.attachment_id.clone(),
                source_hash: placement.source_hash.clone(),
                x: image.x,
                y: image.y,
                width: image.width,
                height: image.height,
            })
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(Text::from(model.sticky_lines)).style(Style::new().fg(FG).bg(BG)),
        sticky_area,
    );
    frame.render_widget(
        Paragraph::new(Text::from(model.lines)).style(Style::new().fg(FG).bg(BG)),
        body_area,
    );
    let body_visible = body_area.height as usize;
    if model.content_height > body_visible {
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .style(Style::new().fg(FG_DIM).bg(BG))
                .thumb_style(Style::new().fg(FG_MUTED)),
            body_area,
            &mut ScrollbarState::new(model.content_height)
                .position(model.scrollbar_position)
                .viewport_content_length(body_visible.max(1)),
        );
    }
    widgets.inline_image_placements.extend(images);
}

#[cfg(test)]
fn detail_content_lines(
    item: &TaskListItem,
    width: usize,
    inline_title_editor: Option<&TextInputView>,
) -> Vec<Line<'static>> {
    let mut lines = detail_header_options(item, width, inline_title_editor);
    lines.extend(detail_body_lines(item, width, None));
    lines
}

#[cfg(test)]
fn detail_body_lines(
    item: &TaskListItem,
    width: usize,
    hovered_child_task_id: Option<&str>,
) -> Vec<Line<'static>> {
    detail_body_lines_with_images(item, width, hovered_child_task_id, None).0
}

#[cfg(test)]
fn detail_body_lines_with_images(
    item: &TaskListItem,
    width: usize,
    hovered_child_task_id: Option<&str>,
    inline_images: Option<&DetailInlineImageContext>,
) -> (
    Vec<Line<'static>>,
    Vec<DetailBodyImagePlacement>,
    Vec<DetailBodyAttachmentPlacement>,
    Vec<DetailInteractiveRow>,
) {
    let target = hovered_child_task_id.map(|task_id| DetailTargetId::Task {
        section: DetailSection::EpicChildren,
        task_id: crate::ids::TaskId::try_from(task_id.to_string()).expect("valid test task ID"),
    });
    detail_body_lines_with_pending_images(
        item,
        width,
        target.as_ref(),
        &BTreeSet::new(),
        inline_images,
        &[],
    )
}

#[cfg(test)]
fn detail_body_lines_with_pending_images(
    item: &TaskListItem,
    width: usize,
    active_target: Option<&DetailTargetId>,
    expanded_sections: &BTreeSet<DetailSection>,
    inline_images: Option<&DetailInlineImageContext>,
    pending_attachments: &[crate::tui::attachment_controller::PendingAttachmentView],
) -> (
    Vec<Line<'static>>,
    Vec<DetailBodyImagePlacement>,
    Vec<DetailBodyAttachmentPlacement>,
    Vec<DetailInteractiveRow>,
) {
    let epic_children = detail_epic_children(item, None);
    let body = build_detail_body_document(
        item,
        &epic_children,
        width,
        expanded_sections,
        inline_images,
        pending_attachments,
    );
    let mut model = project_detail_content_model(Vec::new(), &body, usize::MAX, 0);
    if let Some(active_target) = active_target {
        apply_active_style(&mut model, active_target);
    }
    let attachment_placements = body
        .interactive_rows
        .iter()
        .filter_map(|row| match &row.target {
            DetailTargetId::Attachment { attachment_id } => Some(DetailBodyAttachmentPlacement {
                attachment_id: attachment_id.clone(),
                line_index: row.line_index,
                height: row.height,
            }),
            _ => None,
        })
        .collect();
    (
        model.lines,
        body.image_placements.to_vec(),
        attachment_placements,
        body.interactive_rows.to_vec(),
    )
}

fn build_detail_body_document(
    item: &TaskListItem,
    epic_children: &[DetailEpicChild],
    width: usize,
    expanded_sections: &BTreeSet<DetailSection>,
    inline_images: Option<&DetailInlineImageContext>,
    pending_attachments: &[crate::tui::attachment_controller::PendingAttachmentView],
) -> DetailBodyDocument {
    let mut lines = Vec::new();
    let mut interactive_rows = Vec::new();
    let mut section_body_indices = vec![0];
    extend_epic_parent_section(&mut lines, &mut interactive_rows, item, width, None);
    extend_epic_children_section(
        &mut lines,
        &mut interactive_rows,
        item,
        epic_children,
        width,
        None,
        expanded_sections.contains(&DetailSection::EpicChildren),
    );
    if !lines.is_empty() {
        lines.push(Line::from(""));
    }
    section_body_indices.push(lines.len());

    let mut image_placements = Vec::new();
    let mut hyperlinks = Vec::new();
    let mut selectable_description = Vec::new();
    let mut selectable_text = String::new();
    let description = description_or_placeholder(&item.task.description);
    let content_width = width.saturating_sub(3).max(1);
    let blocks = detail_body_blocks(
        &description,
        content_width,
        MarkdownRenderContext,
        inline_images,
    );
    let rendered_description = blocks
        .iter()
        .map(|block| match block {
            DetailBodyBlock::Line(line) => line.clone(),
            DetailBodyBlock::Image { placeholder, .. } => placeholder.clone(),
        })
        .collect::<Vec<_>>();
    hyperlinks.extend(markdown_hyperlinks(
        &description,
        &rendered_description,
        lines.len(),
        2,
    ));
    for (index, block) in blocks.into_iter().enumerate() {
        let selectable_line = match &block {
            DetailBodyBlock::Line(line) => line.to_string(),
            DetailBodyBlock::Image { placeholder, .. } => placeholder.to_string(),
        };
        if !item.task.description.is_empty() {
            if index > 0 {
                selectable_text.push('\n');
            }
            let document_start = selectable_text.len();
            selectable_text.push_str(&selectable_line);
            selectable_description.push(SelectableLine {
                text: selectable_line,
                document_start,
                body_index: Some(lines.len()),
            });
        }
        match block {
            DetailBodyBlock::Line(line) => {
                lines.push(quoted_line(line, Style::new().fg(FG_MUTED)));
            }
            DetailBodyBlock::Image {
                placeholder,
                attachment_id,
                source_hash,
                width,
                height,
            } => {
                let line_index = lines.len().saturating_add(1);
                lines.push(quoted_line(placeholder, Style::new().fg(FG_MUTED)));
                for _ in 0..height {
                    lines.push(Line::from(vec![Span::styled(
                        "│ ",
                        Style::new().fg(BORDER),
                    )]));
                }
                image_placements.push(DetailBodyImagePlacement {
                    attachment_id,
                    source_hash,
                    line_index,
                    width,
                    height,
                });
            }
        }
    }

    if !item.metadata.is_empty() {
        lines.push(Line::raw(""));
        section_body_indices.push(lines.len());
        interactive_rows.push(DetailInteractiveRow {
            target: DetailTargetId::CustomMetadata,
            line_index: lines.len(),
            height: 1,
        });
        lines.push(Line::from(vec![
            Span::styled(
                "CUSTOM METADATA",
                Style::new().fg(FG_DIM).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" (", Style::new().fg(FG_DIM)),
            Span::styled("e", keycap_style()),
            Span::raw(" "),
            Span::styled("m", keycap_style()),
            Span::styled(" edit)", Style::new().fg(FG_DIM)),
        ]));
        let key_width = item
            .metadata
            .iter()
            .map(|value| value.key.width())
            .max()
            .unwrap_or(0)
            .min(24)
            .min(width / 3);
        for value in &item.metadata {
            let long_key = value.key.width() > key_width;
            if long_key {
                for mut line in plain_metadata_lines(
                    &value.key,
                    width.saturating_sub(2),
                    Style::new().fg(FG_DIM),
                ) {
                    line.spans.insert(0, Span::raw("  "));
                    lines.push(line);
                }
            }
            let indent = if long_key { 4 } else { key_width + 4 };
            let (text, style) = (value.value.as_str(), Style::new().fg(FG));
            for (index, mut line) in plain_metadata_lines(text, width.saturating_sub(indent), style)
                .into_iter()
                .enumerate()
            {
                let prefix = if index == 0 && !long_key {
                    format!("  {:key_width$}  ", value.key)
                } else {
                    " ".repeat(indent)
                };
                line.spans
                    .insert(0, Span::styled(prefix, Style::new().fg(FG_DIM)));
                lines.push(line);
            }
            if value.value.contains('\n') || long_key {
                lines.push(Line::raw(""));
            }
        }
    }

    let mut attachment_placements = Vec::new();
    extend_attachment_section(
        &mut lines,
        &mut image_placements,
        &mut attachment_placements,
        &item.attachments,
        width,
        inline_images,
    );
    for placement in attachment_placements {
        interactive_rows.push(DetailInteractiveRow {
            target: DetailTargetId::Attachment {
                attachment_id: placement.attachment_id,
            },
            line_index: placement.line_index,
            height: placement.height,
        });
    }
    extend_pending_attachment_section(
        &mut lines,
        &item.task.id,
        pending_attachments,
        item.attachments
            .iter()
            .any(|attachment| !attachment.deleted),
    );
    lines.push(Line::from(""));
    section_body_indices.push(lines.len());
    extend_detail_note_section(
        &mut lines,
        &mut interactive_rows,
        &mut hyperlinks,
        item,
        width,
    );
    if item
        .related
        .iter()
        .any(|link| !link.deleted || item.task.deleted)
    {
        let related_start = lines.len();
        extend_related_section(
            &mut lines,
            &mut interactive_rows,
            item,
            width,
            None,
            expanded_sections.contains(&DetailSection::Related),
        );
        section_body_indices.push(related_start.saturating_add(1));
    }
    if !item.depends_on.is_empty() || !item.blocks.is_empty() {
        let dependency_start = lines.len();
        extend_dependency_sections(
            &mut lines,
            &mut interactive_rows,
            item,
            width,
            None,
            expanded_sections,
        );
        section_body_indices.push(dependency_start.saturating_add(1));
    }
    let activity_start = lines.len();
    extend_activity_section(
        &mut lines,
        &mut interactive_rows,
        item,
        width,
        expanded_sections.contains(&DetailSection::Activity),
    );
    section_body_indices.push(activity_start.saturating_add(1));
    section_body_indices.sort_unstable();
    section_body_indices.dedup();

    DetailBodyDocument {
        lines,
        image_placements: Rc::new(image_placements),
        interactive_rows: Rc::new(interactive_rows),
        hyperlinks,
        selectable_description,
        selectable_text,
        section_body_indices,
    }
}

#[cfg(test)]
fn detail_selectable_document(
    item: &TaskListItem,
    width: usize,
    inline_images: Option<&DetailInlineImageContext>,
) -> DetailSelectableDocument {
    let epic_children = detail_epic_children(item, None);
    let body = build_detail_body_document(
        item,
        &epic_children,
        width,
        &BTreeSet::new(),
        inline_images,
        &[],
    );
    detail_selectable_document_from_body(item, width, true, &body)
}

fn detail_selectable_document_from_body(
    item: &TaskListItem,
    width: usize,
    wrap_title: bool,
    body: &DetailBodyDocument,
) -> DetailSelectableDocument {
    let title = if wrap_title {
        title_line_ranges(&item.task.title, width)
            .into_iter()
            .map(|range| SelectableLine {
                text: item.task.title[range.clone()].to_string(),
                document_start: range.start,
                body_index: None,
            })
            .collect()
    } else {
        vec![SelectableLine {
            text: item.task.title.clone(),
            document_start: 0,
            body_index: None,
        }]
    };
    let mut text = item.task.title.clone();
    let mut description = body.selectable_description.clone();
    if !description.is_empty() {
        text.push('\n');
        let description_start = text.len();
        text.push_str(&body.selectable_text);
        for line in &mut description {
            line.document_start += description_start;
        }
    }
    DetailSelectableDocument {
        text,
        title,
        description,
    }
}

fn apply_detail_selection_from_document(
    document: &DetailSelectableDocument,
    selection: &DetailTextSelection,
    sticky_lines: &mut [Line<'static>],
    body_lines: &mut [Line<'static>],
    body_start: usize,
) {
    let range = selection.range();
    for (line, selectable) in sticky_lines.iter_mut().zip(&document.title) {
        highlight_selectable_line(line, selectable, &range, 0);
    }
    let body_end = body_start.saturating_add(body_lines.len());
    let first = document
        .description
        .partition_point(|line| line.body_index.is_some_and(|index| index < body_start));
    let last = document.description[first..]
        .partition_point(|line| line.body_index.is_some_and(|index| index < body_end))
        + first;
    for selectable in &document.description[first..last] {
        if let Some(line) = selectable
            .body_index
            .and_then(|index| index.checked_sub(body_start))
            .and_then(|index| body_lines.get_mut(index))
        {
            highlight_selectable_line(line, selectable, &range, 1);
        }
    }
}

fn highlight_selectable_line(
    line: &mut Line<'static>,
    selectable: &SelectableLine,
    selection: &std::ops::Range<usize>,
    skipped_spans: usize,
) {
    let line_start = selectable.document_start;
    let line_end = line_start + selectable.text.len();
    let start = selection.start.max(line_start).min(line_end) - line_start;
    let end = selection.end.max(line_start).min(line_end) - line_start;
    if start >= end {
        return;
    }

    let mut rebuilt = Vec::new();
    let mut offset = 0;
    for (index, span) in std::mem::take(&mut line.spans).into_iter().enumerate() {
        if index < skipped_spans {
            rebuilt.push(span);
            continue;
        }
        let content = span.content.as_ref();
        let span_start = offset;
        let span_end = offset + content.len();
        let selected_start = start.max(span_start).min(span_end) - span_start;
        let selected_end = end.max(span_start).min(span_end) - span_start;
        if selected_start > 0 {
            rebuilt.push(Span::styled(
                content[..selected_start].to_string(),
                span.style,
            ));
        }
        if selected_start < selected_end {
            rebuilt.push(Span::styled(
                content[selected_start..selected_end].to_string(),
                span.style.fg(INVERSE_FG).bg(ACCENT),
            ));
        }
        if selected_end < content.len() {
            rebuilt.push(Span::styled(
                content[selected_end..].to_string(),
                span.style,
            ));
        }
        offset = span_end;
    }
    line.spans = rebuilt;
}

#[cfg(test)]
fn detail_section_body_indices(
    item: &TaskListItem,
    width: usize,
    inline_images: Option<&DetailInlineImageContext>,
) -> Vec<usize> {
    build_detail_body_document(
        item,
        &detail_epic_children(item, None),
        width,
        &BTreeSet::new(),
        inline_images,
        &[],
    )
    .section_body_indices
}

fn extend_detail_note_section(
    lines: &mut Vec<Line<'static>>,
    rows: &mut Vec<DetailInteractiveRow>,
    hyperlinks: &mut Vec<DetailHyperlink>,
    item: &TaskListItem,
    width: usize,
) {
    let mut header = vec![
        Span::styled(
            "NOTES",
            Style::new().fg(FG_DIM).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" (", Style::new().fg(FG_DIM)),
        Span::styled("n", keycap_style()),
        Span::styled(" add", Style::new().fg(FG_DIM)),
    ];
    if !item.notes.is_empty() && width >= 42 {
        header.extend([
            Span::styled(" · ", Style::new().fg(FG_DIM)),
            Span::styled("e", keycap_style()),
            Span::styled(" edit · ", Style::new().fg(FG_DIM)),
            Span::styled("D", keycap_style()),
            Span::styled(" delete", Style::new().fg(FG_DIM)),
        ]);
    }
    header.push(Span::styled(")", Style::new().fg(FG_DIM)));
    lines.push(Line::from(header));
    if item.notes.is_empty() {
        lines.push(Line::from(Span::styled("none", Style::new().fg(FG_MUTED))));
    } else {
        for note in &item.notes {
            lines.push(Line::from(""));
            let mut rendered = vec![Line::from(Span::styled(
                local_timestamp_display(&note.created_at),
                Style::new().fg(FG_DIM),
            ))];
            let note_lines = quoted_block_lines(&note.body, width, Style::new().fg(FG));
            let unquoted_note_lines =
                render_markdown_without_link_urls(&note.body, width.saturating_sub(3).max(1));
            hyperlinks.extend(markdown_hyperlinks(
                &note.body,
                &unquoted_note_lines,
                lines.len().saturating_add(1),
                2,
            ));
            rendered.extend(note_lines);
            push_interactive_lines(
                lines,
                rows,
                DetailTargetId::Note {
                    note_id: note.id.clone(),
                },
                rendered,
            );
        }
    }
}

fn extend_activity_section(
    lines: &mut Vec<Line<'static>>,
    rows: &mut Vec<DetailInteractiveRow>,
    item: &TaskListItem,
    width: usize,
    expanded: bool,
) {
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "ACTIVITY",
        Style::new().fg(FG_DIM).add_modifier(Modifier::BOLD),
    )));
    if item.activity.is_empty() {
        lines.push(Line::from(Span::styled(
            "No recorded task activity.",
            Style::new().fg(FG_MUTED),
        )));
        return;
    }

    let disclosure = DetailTargetId::Expand {
        section: DetailSection::Activity,
    };
    if !expanded {
        let count = item.activity.len();
        let label = format!(
            "Show {count} {}",
            if count == 1 { "event" } else { "events" }
        );
        push_disclosure_row(lines, rows, disclosure, &label, None);
        return;
    }

    let available = item.queue.band == crate::queue::QueueBand::Available;
    let idle_index = item.queue_idle_activity_index();
    let idle_tag = item
        .queue
        .idle_seconds
        .map(crate::tui::time::compact_duration)
        .map(|duration| format!("idle {duration}"));
    let anchored_idle = idle_index
        .zip(idle_tag.as_deref())
        .filter(|(_, tag)| width.saturating_sub(19 + UnicodeWidthStr::width(*tag) + 2) >= 20);
    if available && let Some(tag) = idle_tag.as_deref() {
        lines.push(Line::from(Span::styled(
            truncate_width(&format!("{tag} · since becoming available"), width),
            Style::new().fg(FG_DIM),
        )));
    } else if anchored_idle.is_none()
        && let Some((index, tag)) = idle_index.zip(idle_tag.as_deref())
    {
        lines.push(Line::from(Span::styled(
            truncate_width(
                &format!(
                    "{tag} · since latest {}",
                    idle_activity_noun(&item.activity[index])
                ),
                width,
            ),
            Style::new().fg(FG_DIM),
        )));
    } else if idle_index.is_none()
        && let Some(tag) = idle_tag.as_deref()
    {
        lines.push(Line::from(Span::styled(
            truncate_width(&format!("{tag} · based on recent task activity"), width),
            Style::new().fg(FG_DIM),
        )));
    }

    for (index, action) in item.activity.iter().enumerate() {
        let timestamp = local_activity_timestamp_display(&action.created_at);
        let icon = super::recent_actions::action_icon(action);
        let prefix_width =
            2 + UnicodeWidthStr::width(timestamp.as_str()) + 2 + UnicodeWidthStr::width(icon) + 1;
        let show_idle = anchored_idle.is_some_and(|(idle_index, _)| idle_index == index);
        let reserved = if show_idle {
            idle_tag
                .as_deref()
                .map(|tag| UnicodeWidthStr::width(tag) + 2)
                .unwrap_or(0)
        } else {
            0
        };
        let summary = truncate_width(
            &action.task_activity_summary(&item.task.title),
            width.saturating_sub(prefix_width + reserved),
        );
        let mut spans = vec![
            Span::raw("  "),
            Span::styled(timestamp, Style::new().fg(FG_DIM)),
            Span::raw("  "),
            Span::styled(icon, super::recent_actions::action_style(action)),
            Span::raw(" "),
            Span::styled(summary.clone(), Style::new().fg(FG)),
        ];
        if show_idle && let Some(tag) = idle_tag.as_deref() {
            let padding = width.saturating_sub(
                prefix_width
                    + UnicodeWidthStr::width(summary.as_str())
                    + UnicodeWidthStr::width(tag),
            );
            spans.push(Span::raw(" ".repeat(padding.max(2))));
            spans.push(Span::styled(tag.to_string(), Style::new().fg(FG_DIM)));
        }
        lines.push(Line::from(spans));
    }
    push_disclosure_row(lines, rows, disclosure, "Hide activity", None);
}

fn idle_activity_noun(action: &crate::query::RecentActionItem) -> &'static str {
    match action.op_type.as_str() {
        crate::change_log::op_type::CREATE_TASK => "creation",
        crate::change_log::op_type::NOTE_ADD
        | crate::change_log::op_type::NOTE_EDIT
        | crate::change_log::op_type::NOTE_DELETE => "note",
        _ if action.field.as_deref() == Some("priority") => "priority change",
        _ => "status change",
    }
}

fn push_interactive_lines(
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

fn extend_epic_parent_section(
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

fn epic_child_counts(children: &[DetailEpicChild]) -> EpicChildCounts {
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

fn ordered_epic_children(children: &[DetailEpicChild]) -> Vec<&DetailEpicChild> {
    children
        .iter()
        .filter(|child| child.link.unresolved)
        .chain(children.iter().filter(|child| !child.link.unresolved))
        .collect()
}

fn extend_epic_children_section(
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

fn push_disclosure_row(
    lines: &mut Vec<Line<'static>>,
    rows: &mut Vec<DetailInteractiveRow>,
    target: DetailTargetId,
    label: &str,
    active_target: Option<&DetailTargetId>,
) {
    let active = active_target == Some(&target);
    push_interactive_lines(lines, rows, target, vec![disclosure_line(label, active)]);
}

fn disclosure_line(label: &str, active: bool) -> Line<'static> {
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

fn epic_child_tree_item_lines(
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

fn epic_child_dependency_lines(
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

fn extend_dependency_sections(
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
fn extend_dependency_section(
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

fn extend_related_section(
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

fn apply_link_row_style(lines: &mut [Line<'static>]) {
    for line in lines {
        for span in &mut line.spans {
            span.style = span.style.bg(BG_PANEL);
        }
    }
}

#[cfg(test)]
fn detail_dependency_lines(item: &TaskListItem, width: usize) -> Vec<Line<'static>> {
    if item.depends_on.is_empty() && item.blocks.is_empty() {
        return Vec::new();
    }

    let mut lines = vec![Line::from("")];

    if !item.depends_on.is_empty() {
        lines.push(dependency_heading("WHY BLOCKED", &item.depends_on));
        lines.extend(dependency_branch_lines(
            &item.depends_on,
            DependencyDirection::Blocker,
            width,
        ));
    }

    if !item.blocks.is_empty() {
        lines.push(Line::from(""));
        lines.push(dependency_heading("WHAT THIS UNLOCKS", &item.blocks));
        lines.extend(dependency_branch_lines(
            &item.blocks,
            DependencyDirection::Dependent,
            width,
        ));
    }

    lines
}

#[cfg(test)]
fn dependency_branch_lines(
    links: &[crate::query::TaskDependencyLink],
    direction: DependencyDirection,
    width: usize,
) -> Vec<Line<'static>> {
    let visible = links.len().min(DETAIL_DEPENDENCY_TREE_CAP);
    let hidden = links.len().saturating_sub(visible);
    let rendered_len = visible + usize::from(hidden > 0);
    let mut lines = Vec::with_capacity(rendered_len);

    for (index, link) in links.iter().take(visible).enumerate() {
        let is_last = index + 1 == rendered_len;
        lines.extend(dependency_tree_item_lines(link, direction, is_last, width));
    }

    if hidden > 0 {
        lines.push(Line::from(vec![
            Span::styled("└─ ", Style::new().fg(BORDER)),
            Span::styled(format!("+{hidden} more"), Style::new().fg(FG_MUTED)),
        ]));
    }

    lines
}

fn dependency_heading(
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

fn dependency_tree_item_lines(
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

fn dependency_node_lines(
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

fn dependency_node_lines_with_title_style(
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

fn detail_header_options(
    item: &TaskListItem,
    width: usize,
    inline_title_editor: Option<&TextInputView>,
) -> Vec<Line<'static>> {
    let mut summary_spans = vec![
        Span::styled(
            "● ",
            Style::new().fg(theme::project_color(&item.task.project_key)),
        ),
        Span::styled(
            item.task.project_key.clone(),
            Style::new().fg(FG).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" / ", Style::new().fg(FG_DIM)),
    ];
    summary_spans.extend(linked_task_ref_spans(
        &item.display_ref,
        &item.task.project_key,
    ));
    if item.task.is_epic {
        summary_spans.extend([
            Span::styled("  ", Style::new().fg(FG_DIM)),
            Span::styled(EPIC_MARKER, Style::new().fg(YELLOW)),
        ]);
    }
    summary_spans.extend([
        Span::styled("   ", Style::new().fg(FG_DIM)),
        status_span(item.task.status.as_str()),
        Span::styled("   ", Style::new().fg(FG_DIM)),
        Span::styled(
            priority_short(item.task.priority.as_str()),
            theme::priority_style(item.task.priority.as_str()).add_modifier(Modifier::BOLD),
        ),
    ]);
    let mut lines = detail_title_lines(item, width, inline_title_editor);
    lines.extend([
        Line::from(Span::styled("─".repeat(width), Style::new().fg(BORDER))),
        Line::from(summary_spans),
        Line::from(""),
    ]);
    lines
}

fn detail_title_lines(
    item: &TaskListItem,
    width: usize,
    inline_title_editor: Option<&TextInputView>,
) -> Vec<Line<'static>> {
    if let Some(editor) = inline_title_editor {
        let mut line = clipped_input_line(&editor.input, editor.cursor, width);
        for span in &mut line.spans {
            span.style = Style::new()
                .fg(FG)
                .add_modifier(Modifier::BOLD)
                .patch(span.style);
        }
        return vec![line];
    }

    title_line_ranges(&item.task.title, width)
        .into_iter()
        .map(|range| {
            Line::from(Span::styled(
                item.task.title[range].to_string(),
                Style::new().fg(FG).add_modifier(Modifier::BOLD),
            ))
        })
        .collect()
}

fn title_line_ranges(title: &str, width: usize) -> Vec<std::ops::Range<usize>> {
    let width = width.max(1);
    let mut words = Vec::new();
    let mut word_start = None;
    for (index, character) in title.char_indices() {
        if character.is_whitespace() {
            if let Some(start) = word_start.take() {
                words.push(start..index);
            }
        } else if word_start.is_none() {
            word_start = Some(index);
        }
    }
    if let Some(start) = word_start {
        words.push(start..title.len());
    }
    if words.is_empty() {
        return std::iter::once(0..title.len()).collect();
    }

    let mut lines = Vec::new();
    let mut current: Option<std::ops::Range<usize>> = None;
    for word in words {
        if let Some(line) = &mut current {
            let candidate = line.start..word.end;
            if title[candidate.clone()].width() <= width {
                line.end = word.end;
                continue;
            }
            lines.push(line.clone());
            current = None;
        }

        let mut chunk_start = word.start;
        for (offset, character) in title[word.clone()].char_indices() {
            let index = word.start + offset;
            let end = index + character.len_utf8();
            if title[chunk_start..end].width() <= width {
                continue;
            }
            if chunk_start < index {
                lines.push(chunk_start..index);
                chunk_start = index;
            }
            if title[chunk_start..end].width() > width {
                lines.push(chunk_start..end);
                chunk_start = end;
            }
        }
        if chunk_start < word.end {
            current = Some(chunk_start..word.end);
        }
    }
    if let Some(line) = current {
        lines.push(line);
    }
    lines
}

struct ParsedMarkdownLink {
    label: String,
    url: String,
}

fn markdown_links(markdown: &str) -> Vec<ParsedMarkdownLink> {
    let mut links = Vec::new();
    let mut current = None;
    for event in Parser::new(markdown) {
        match event {
            Event::Start(Tag::Link { dest_url, .. }) => {
                current = Some(ParsedMarkdownLink {
                    label: String::new(),
                    url: dest_url.to_string(),
                });
            }
            Event::Text(text) | Event::Code(text) => {
                if let Some(link) = &mut current {
                    link.label.push_str(&text);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some(link) = &mut current {
                    link.label.push(' ');
                }
            }
            Event::End(TagEnd::Link) => {
                if let Some(link) = current.take() {
                    links.push(link);
                }
            }
            _ => {}
        }
    }
    links
}

fn markdown_hyperlinks(
    markdown: &str,
    lines: &[Line<'static>],
    line_offset: usize,
    column_offset: usize,
) -> Vec<DetailHyperlink> {
    let links = markdown_links(markdown);
    let mut placements = Vec::new();
    let mut link_index = 0;
    let mut rendered_width = 0usize;
    for (line_index, line) in lines.iter().enumerate() {
        let mut column = column_offset;
        for span in &line.spans {
            let width = span.content.width();
            if span.style.add_modifier.contains(Modifier::UNDERLINED)
                && let Some(link) = links.get(link_index)
            {
                if (link.url.starts_with("https://") || link.url.starts_with("http://"))
                    && width > 0
                {
                    placements.push(DetailHyperlink {
                        url: link.url.clone(),
                        line_index: line_offset.saturating_add(line_index),
                        start_column: column,
                        end_column: column.saturating_add(width),
                    });
                }
                rendered_width = rendered_width.saturating_add(width);
                if rendered_width >= link.label.width() {
                    link_index = link_index.saturating_add(1);
                    rendered_width = 0;
                }
            }
            column = column.saturating_add(width);
        }
    }
    placements
}

fn plain_metadata_lines(value: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    let width = width.max(1);
    value
        .split('\n')
        .flat_map(|line| {
            let mut lines = Vec::new();
            let mut text = String::new();
            let mut cells = 0;
            for c in line.chars() {
                let shown = if c.is_control() { '�' } else { c };
                let size = unicode_width::UnicodeWidthChar::width(shown).unwrap_or(0);
                if cells + size > width && cells > 0 {
                    lines.push(Line::styled(std::mem::take(&mut text), style));
                    cells = 0;
                }
                text.push(shown);
                cells += size;
            }
            lines.push(Line::styled(text, style));
            lines
        })
        .collect()
}

fn quoted_block_lines(body: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    let content_width = width.saturating_sub(3).max(1);
    render_markdown_without_link_urls(body, content_width)
        .into_iter()
        .map(|line| {
            let mut spans = line_with_base_style(line, style).spans;
            spans.insert(0, Span::styled("│ ", Style::new().fg(BORDER)));
            Line::from(spans)
        })
        .collect()
}

fn extend_pending_attachment_section(
    lines: &mut Vec<Line<'static>>,
    task_id: &crate::ids::TaskId,
    pending_attachments: &[crate::tui::attachment_controller::PendingAttachmentView],
    has_live_attachments: bool,
) {
    let pending = pending_attachments
        .iter()
        .filter(|attachment| &attachment.task_id == task_id)
        .collect::<Vec<_>>();
    if pending.is_empty() {
        return;
    }
    if !has_live_attachments {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "ATTACHMENTS",
            Style::new().fg(FG_DIM).add_modifier(Modifier::BOLD),
        )));
    }
    for attachment in pending {
        let (label, color) = match attachment.status {
            crate::tui::attachment_controller::PendingAttachmentStatus::Preparing => {
                ("[image: preparing]", FG_MUTED)
            }
            crate::tui::attachment_controller::PendingAttachmentStatus::Failed => {
                ("[image: failed]", crate::tui::theme::RED)
            }
        };
        lines.push(quoted_line(Line::from(label), Style::new().fg(color)));
    }
}

fn extend_attachment_section(
    lines: &mut Vec<Line<'static>>,
    placements: &mut Vec<DetailBodyImagePlacement>,
    attachment_placements: &mut Vec<DetailBodyAttachmentPlacement>,
    attachments: &[AttachmentMetadataJson],
    width: usize,
    inline_images: Option<&DetailInlineImageContext>,
) {
    let live = attachments
        .iter()
        .filter(|attachment| !attachment.deleted)
        .collect::<Vec<_>>();
    if live.is_empty() {
        return;
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "ATTACHMENTS",
        Style::new().fg(FG_DIM).add_modifier(Modifier::BOLD),
    )));
    let content_width = width.saturating_sub(3).max(1);
    for attachment in live {
        match attachment_detail_block(attachment, content_width, inline_images) {
            DetailBodyBlock::Line(line) => {
                let focused = inline_images.is_some_and(|context| {
                    context.focused_attachment_id.as_deref()
                        == Some(attachment.attachment_id.as_str())
                });
                attachment_placements.push(DetailBodyAttachmentPlacement {
                    attachment_id: attachment.attachment_id.clone(),
                    line_index: lines.len(),
                    height: 1,
                });
                lines.push(quoted_line(
                    line,
                    Style::new().fg(if focused { ACCENT } else { FG_MUTED }),
                ));
            }
            DetailBodyBlock::Image {
                placeholder,
                attachment_id,
                source_hash,
                width,
                height,
            } => {
                let focused = inline_images.is_some_and(|context| {
                    context.focused_attachment_id.as_deref() == Some(attachment_id.as_str())
                });
                attachment_placements.push(DetailBodyAttachmentPlacement {
                    attachment_id: attachment_id.clone(),
                    line_index: lines.len(),
                    height: height as usize + 3,
                });
                let frame_style = Style::new().fg(if focused { ACCENT } else { BORDER });
                lines.push(quoted_line(placeholder, Style::new().fg(FG_MUTED)));
                lines.push(quoted_line(
                    Line::from(format!("┌{}┐", "─".repeat(width as usize))),
                    frame_style,
                ));
                let line_index = lines.len();
                for _ in 0..height {
                    lines.push(quoted_line(
                        Line::from(vec![
                            Span::styled("│", frame_style),
                            Span::raw(" ".repeat(width as usize)),
                            Span::styled("│", frame_style),
                        ]),
                        Style::new().fg(FG_MUTED),
                    ));
                }
                lines.push(quoted_line(
                    Line::from(format!("└{}┘", "─".repeat(width as usize))),
                    frame_style,
                ));
                placements.push(DetailBodyImagePlacement {
                    attachment_id,
                    source_hash,
                    line_index,
                    width,
                    height,
                });
            }
        }
    }
}

pub(crate) fn attachment_is_locally_openable(attachment: &AttachmentMetadataJson) -> bool {
    !attachment.deleted
        && attachment.has_blob
        && attachment.bytes_state == crate::attachments::AttachmentBytesState::Present
        && matches!(
            attachment.media_type.as_str(),
            "image/png" | "image/jpeg" | "image/gif" | "image/webp"
        )
}

pub(crate) fn attachment_is_locally_previewable(
    attachment: &AttachmentMetadataJson,
    unavailable_hashes: &HashSet<String>,
) -> bool {
    attachment_is_locally_openable(attachment)
        && !unavailable_hashes.contains(&attachment.sha256)
        && matches!(
            (attachment.width, attachment.height),
            (Some(width), Some(height)) if width > 0 && height > 0
        )
}

fn attachment_detail_block(
    attachment: &AttachmentMetadataJson,
    content_width: usize,
    inline_images: Option<&DetailInlineImageContext>,
) -> DetailBodyBlock {
    let focused = inline_images.is_some_and(|context| {
        context.focused_attachment_id.as_deref() == Some(attachment.attachment_id.as_str())
    });
    let placeholder = attachment_detail_line(attachment, content_width, focused);
    let Some(inline_images) = inline_images else {
        return DetailBodyBlock::Line(placeholder);
    };
    if !inline_images.previews_enabled
        || !attachment_is_locally_previewable(attachment, &inline_images.unavailable_hashes)
    {
        return DetailBodyBlock::Line(placeholder);
    }
    let (width, height) = image_preview_size(attachment, content_width.saturating_sub(4));
    DetailBodyBlock::Image {
        placeholder,
        attachment_id: attachment.attachment_id.clone(),
        source_hash: attachment.sha256.clone(),
        width,
        height,
    }
}

fn attachment_detail_line(
    attachment: &AttachmentMetadataJson,
    content_width: usize,
    focused: bool,
) -> Line<'static> {
    let state_style = Style::new().fg(if focused { ACCENT } else { FG_MUTED });
    let filename_style = Style::new().fg(if focused { ACCENT } else { FG });
    let separator_style = Style::new().fg(if focused { ACCENT } else { FG_DIM });
    let metadata_style = Style::new().fg(if focused { ACCENT } else { FG_MUTED });
    let mut spans = vec![Span::styled(
        attachment_state_placeholder(attachment),
        state_style,
    )];
    if let Some(filename) = attachment.filename.as_deref() {
        spans.push(Span::styled(format!(" {filename}"), filename_style));
    }
    if let (Some(width), Some(height)) = (attachment.width, attachment.height) {
        spans.push(Span::styled(" · ", separator_style));
        spans.push(Span::styled(format!("{width}×{height}"), metadata_style));
    }
    spans.push(Span::styled(" · ", separator_style));
    spans.push(Span::styled(
        human_file_size(attachment.byte_size),
        metadata_style,
    ));
    truncate_styled_line(Line::from(spans), content_width)
}

fn truncate_styled_line(line: Line<'static>, max_width: usize) -> Line<'static> {
    truncate_line_width(line, max_width, Style::default())
}

fn detail_body_blocks(
    body: &str,
    content_width: usize,
    context: MarkdownRenderContext,
    _inline_images: Option<&DetailInlineImageContext>,
) -> Vec<DetailBodyBlock> {
    render_markdown_with_context_without_link_urls(body, content_width, context)
        .into_iter()
        .map(|block| match block {
            MarkdownBlock::Text(line) => DetailBodyBlock::Line(line),
        })
        .collect()
}

fn image_preview_size(attachment: &AttachmentMetadataJson, content_width: usize) -> (u16, u16) {
    const MAX_HEIGHT_ROWS: u16 = 12;
    const DEFAULT_HEIGHT_ROWS: u16 = 6;
    const CELL_HEIGHT_TO_WIDTH_RATIO: f64 = 2.0;

    let max_width = content_width.clamp(1, u16::MAX as usize) as u16;
    match (attachment.width, attachment.height) {
        (Some(width), Some(height)) if width > 0 && height > 0 => {
            let image_aspect = width as f64 / height as f64;
            let width_at_max_height =
                (MAX_HEIGHT_ROWS as f64 * image_aspect * CELL_HEIGHT_TO_WIDTH_RATIO)
                    .round()
                    .max(1.0) as u16;
            if width_at_max_height <= max_width {
                (width_at_max_height, MAX_HEIGHT_ROWS)
            } else {
                let height = ((max_width as f64 / image_aspect) / CELL_HEIGHT_TO_WIDTH_RATIO)
                    .round()
                    .clamp(3.0, MAX_HEIGHT_ROWS as f64) as u16;
                (max_width, height)
            }
        }
        _ => (max_width.min(80), DEFAULT_HEIGHT_ROWS),
    }
}

fn quoted_line(line: Line<'static>, style: Style) -> Line<'static> {
    let mut spans = line_with_base_style(line, style).spans;
    spans.insert(0, Span::styled("│ ", Style::new().fg(BORDER)));
    Line::from(spans)
}

fn line_with_base_style(mut line: Line<'static>, base: Style) -> Line<'static> {
    for span in &mut line.spans {
        span.style = base.patch(span.style);
    }
    line
}

fn render_detail_metadata(
    frame: &mut Frame,
    item: &TaskListItem,
    epic_children: &[DetailEpicChild],
    area: Rect,
) {
    let block = Block::new()
        .borders(Borders::LEFT)
        .border_style(Style::new().fg(BORDER))
        .padding(Padding::horizontal(1))
        .style(Style::new().bg(BG));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(Text::from(detail_metadata_lines_with_children(
            item,
            epic_children,
            inner.width as usize,
        )))
        .style(Style::new().fg(FG).bg(BG)),
        inner,
    );
}

#[cfg(test)]
fn detail_metadata_lines(item: &TaskListItem, width: usize) -> Vec<Line<'static>> {
    detail_metadata_lines_with_children(item, &detail_epic_children(item, None), width)
}

fn detail_metadata_lines_with_children(
    item: &TaskListItem,
    epic_children: &[DetailEpicChild],
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled(
            if item.task.is_epic {
                " EPIC "
            } else {
                " TASK "
            },
            Style::new()
                .fg(INVERSE_FG)
                .bg(BORDER)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        metadata_label("PROJECT"),
        Line::from(vec![
            Span::styled(
                "● ",
                Style::new().fg(theme::project_color(&item.task.project_key)),
            ),
            Span::styled(
                item.task.project_key.clone(),
                Style::new().fg(FG).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        metadata_label("STATUS"),
        status_chip(item.task.status.as_str()),
        Line::from(""),
        metadata_label("PRIORITY"),
        Line::from(Span::styled(
            priority_short(item.task.priority.as_str()),
            theme::priority_style(item.task.priority.as_str()).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        metadata_label("LABELS"),
        Line::from(labels_display(&item.labels, ", ")),
    ];
    let now_seconds = crate::queue::now_seconds();
    let availability = crate::tui::time::availability_summary_lines(
        item.task.available_at.as_deref().unwrap_or(""),
        item.queue.band == crate::queue::QueueBand::Available,
        now_seconds,
    );
    let availability_style = if availability.is_some() {
        Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(FG_MUTED)
    };
    lines.extend([Line::from(""), metadata_label("AVAILABILITY")]);
    for value in availability
        .map(Vec::from)
        .unwrap_or_else(|| vec!["none".to_string()])
    {
        lines.push(Line::from(Span::styled(
            truncate_width(&value, width),
            availability_style,
        )));
    }
    let due =
        crate::tui::time::due_summary_lines(item.task.due_on.as_deref().unwrap_or(""), now_seconds);
    let due_color = if item.task.due_on.is_none() || !item.task.status.is_open() {
        FG_MUTED
    } else {
        match crate::tui::time::due_state_at(item.task.due_on.as_deref().unwrap_or(""), now_seconds)
        {
            crate::due::DueState::Overdue(_) => RED,
            crate::due::DueState::Today => ORANGE,
            crate::due::DueState::Future(_) => ACCENT,
            crate::due::DueState::None => FG_MUTED,
        }
    };
    let due_style = Style::new().fg(due_color).add_modifier(Modifier::BOLD);
    lines.extend([Line::from(""), metadata_label("DUE")]);
    for value in due
        .map(Vec::from)
        .unwrap_or_else(|| vec!["none".to_string()])
    {
        lines.push(Line::from(Span::styled(
            truncate_width(&value, width),
            due_style,
        )));
    }
    lines.extend([
        Line::from(""),
        metadata_label("REF"),
        Line::from(Span::styled(
            item.display_ref.clone(),
            Style::new().fg(FG).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        metadata_label("CREATED"),
        Line::from(Span::styled(
            local_timestamp_display(&item.task.created_at),
            Style::new().fg(FG_MUTED),
        )),
        Line::from(""),
        metadata_label("UPDATED"),
        Line::from(Span::styled(
            local_timestamp_display(&item.task.updated_at),
            Style::new().fg(FG_MUTED),
        )),
    ]);
    if let Some(recurrence) = item.recurrence.as_ref() {
        let outcome = recurrence
            .outcome
            .map(|value| value.as_str())
            .unwrap_or("open");
        lines.extend([
            Line::from(""),
            metadata_label("RECURRENCE"),
            Line::from(Span::styled(
                format!("↻ {}", recurrence.series_ref),
                Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
            )),
            Line::from(format!("schedule {}", recurrence.rule_label)),
            Line::from(format!("slot {}", recurrence.slot_on)),
            Line::from(format!("zone {}", recurrence.timezone)),
            Line::from(format!("lifecycle {}", recurrence.lifecycle.as_str())),
            Line::from(format!("outcome {outcome}")),
            Line::from(format!(
                "projection {}",
                recurrence.projection_state.as_str()
            )),
            Line::from(Span::styled("history t r h", Style::new().fg(FG_MUTED))),
        ]);
    }
    if let Some(group) = item.recurrence_group.as_ref() {
        lines.extend([
            Line::from(""),
            metadata_label("SERIES HISTORY"),
            Line::from(format!("completed {}", group.counts.completed)),
            Line::from(format!("skipped {}", group.counts.skipped)),
            Line::from(format!("missed {}", group.counts.missed)),
        ]);
    }
    lines.extend(detail_epic_metadata_lines(item, epic_children));
    if item.has_conflict {
        lines.extend([
            Line::from(""),
            metadata_label("CONFLICTS"),
            Line::from(Span::styled(
                "yes",
                Style::new().fg(ORANGE).add_modifier(Modifier::BOLD),
            )),
        ]);
    }
    if item.task.deleted {
        lines.extend([
            Line::from(""),
            metadata_label("DELETED"),
            Line::from(Span::styled(
                "yes",
                Style::new().fg(RED).add_modifier(Modifier::BOLD),
            )),
        ]);
    }
    lines
}

fn detail_epic_metadata_lines(
    item: &TaskListItem,
    children: &[DetailEpicChild],
) -> Vec<Line<'static>> {
    if !item.task.is_epic {
        return Vec::new();
    }

    let counts = epic_child_counts(children);
    let progress = item.epic_rollup.as_ref().map_or_else(
        || format!("open={} total={}", counts.open, counts.total),
        |rollup| {
            format!(
                "{} open · {} done · {} canceled",
                rollup.open, rollup.done, rollup.canceled
            )
        },
    );
    let mut lines = vec![
        Line::from(""),
        metadata_label("CHILDREN"),
        Line::from(Span::styled(progress, Style::new().fg(FG_DIM))),
    ];
    if let Some(rollup) = item.epic_rollup.as_ref()
        && rollup.total > 0
    {
        lines.push(Line::from(Span::styled(
            format!(
                "{} overdue · {} blocked · {} ready",
                rollup.overdue, rollup.blocked, rollup.ready
            ),
            Style::new().fg(FG_DIM),
        )));
    }

    if children.is_empty() {
        return lines
            .into_iter()
            .chain(std::iter::once(Line::from(Span::styled(
                "none",
                Style::new().fg(FG_MUTED),
            ))))
            .collect();
    }

    lines
}

fn metadata_label(label: &'static str) -> Line<'static> {
    Line::from(Span::styled(
        label,
        Style::new().fg(FG_DIM).add_modifier(Modifier::BOLD),
    ))
}

#[cfg(test)]
pub(crate) fn detail_text_cell_at_position(
    item: &TaskListItem,
    terminal_width: u16,
    terminal_height: u16,
    column: u16,
    row: u16,
    scroll: u16,
) -> Option<TextCell> {
    let expanded_sections = BTreeSet::new();
    DetailDocument::build(
        item,
        &detail_query_context(
            terminal_width,
            terminal_height,
            scroll,
            &expanded_sections,
            None,
        ),
    )
    .text_cell_at_position(column, row)
}

pub(crate) fn detail_selected_text(
    item: &TaskListItem,
    selection: &DetailTextSelection,
) -> Option<String> {
    let expanded_sections = BTreeSet::new();
    DetailDocument::build(
        item,
        &detail_query_context(selection.terminal_width, 24, 0, &expanded_sections, None),
    )
    .selected_text(selection)
}

#[cfg(test)]
pub(crate) fn detail_attachment_at_position(
    item: &TaskListItem,
    terminal_width: u16,
    terminal_height: u16,
    column: u16,
    row: u16,
    scroll: u16,
    inline_images: &DetailInlineImageContext,
) -> Option<DetailAttachmentHit> {
    let expanded_sections = BTreeSet::new();
    DetailDocument::build(
        item,
        &detail_query_context(
            terminal_width,
            terminal_height,
            scroll,
            &expanded_sections,
            Some(inline_images),
        ),
    )
    .attachment_at_position(item, column, row)
    .map(|attachment_id| DetailAttachmentHit { attachment_id })
}

#[cfg(test)]
pub(crate) fn detail_child_task_at_position(
    item: &TaskListItem,
    terminal_width: u16,
    terminal_height: u16,
    column: u16,
    row: u16,
    scroll: u16,
) -> Option<DetailChildHit> {
    let expanded_sections = BTreeSet::new();
    DetailDocument::build(
        item,
        &detail_query_context(
            terminal_width,
            terminal_height,
            scroll,
            &expanded_sections,
            None,
        ),
    )
    .child_task_at_position(column, row)
    .map(|task_id| DetailChildHit { task_id })
}

pub(crate) fn detail_copy_target_at(
    item: &TaskListItem,
    terminal_width: u16,
    terminal_height: u16,
    column: u16,
    row: u16,
) -> Option<DetailCopyHit> {
    let layout = detail_content_layout(Rect::new(0, 0, terminal_width, terminal_height));
    let header_ref_row = layout.content_area.y.saturating_add(2);
    if row == header_ref_row
        && column >= layout.content_area.x
        && column
            < layout
                .content_area
                .x
                .saturating_add(UnicodeWidthStr::width(item.display_ref.as_str()) as u16)
    {
        return Some(DetailCopyHit {
            value: item.display_ref.clone(),
        });
    }

    if layout.metadata_area.width == 0 {
        return None;
    }
    let body = detail_body_area(Rect::new(0, 0, terminal_width, terminal_height));
    let line = metadata_content_row(layout.metadata_area, body, column, row)?;
    let rows = detail_metadata_rows(item);
    let value = match line {
        value if value == rows.reference => item.display_ref.clone(),
        value if value == rows.created => local_timestamp_display(&item.task.created_at),
        value if value == rows.updated => local_timestamp_display(&item.task.updated_at),
        _ => return None,
    };
    let value_start = layout.metadata_area.x.saturating_add(2);
    let value_end = value_start.saturating_add(UnicodeWidthStr::width(value.as_str()) as u16);
    (column >= value_start && column < value_end).then_some(DetailCopyHit { value })
}

pub(crate) fn detail_metadata_target_at(
    item: &TaskListItem,
    terminal_width: u16,
    terminal_height: u16,
    column: u16,
    row: u16,
) -> Option<(DetailMetadataTarget, u16, u16)> {
    let layout = detail_content_layout(Rect::new(0, 0, terminal_width, terminal_height));
    if layout.metadata_area.width == 0 {
        return None;
    }
    let body = detail_body_area(Rect::new(0, 0, terminal_width, terminal_height));
    let line = metadata_content_row(layout.metadata_area, body, column, row)?;
    let rows = detail_metadata_rows(item);
    let target = match line {
        3 => DetailMetadataTarget::Project,
        6 => DetailMetadataTarget::Status,
        9 => DetailMetadataTarget::Priority,
        12 => DetailMetadataTarget::Labels,
        value if rows.availability.contains(&value) => DetailMetadataTarget::Availability,
        value if rows.due.contains(&value) => DetailMetadataTarget::Due,
        _ => return None,
    };
    Some((target, column, row))
}

struct DetailMetadataRows {
    availability: std::ops::Range<u16>,
    due: std::ops::Range<u16>,
    reference: u16,
    created: u16,
    updated: u16,
}

fn detail_metadata_rows(item: &TaskListItem) -> DetailMetadataRows {
    let now_seconds = crate::queue::now_seconds();
    let availability_len = u16::from(
        crate::tui::time::availability_summary_lines(
            item.task.available_at.as_deref().unwrap_or(""),
            item.queue.band == crate::queue::QueueBand::Available,
            now_seconds,
        )
        .is_some(),
    ) + 1;
    let due_len = u16::from(
        crate::tui::time::due_summary_lines(item.task.due_on.as_deref().unwrap_or(""), now_seconds)
            .is_some(),
    ) + 1;
    let availability_start = 15;
    let due_start = availability_start + availability_len + 2;
    let reference = due_start + due_len + 2;

    DetailMetadataRows {
        availability: availability_start..availability_start + availability_len,
        due: due_start..due_start + due_len,
        reference,
        created: reference + 3,
        updated: reference + 6,
    }
}

fn metadata_content_row(metadata_area: Rect, body: Rect, column: u16, row: u16) -> Option<u16> {
    if column <= metadata_area.x
        || column >= metadata_area.x.saturating_add(metadata_area.width)
        || row < body.y
        || row >= body.y.saturating_add(body.height)
    {
        return None;
    }
    Some(row.saturating_sub(body.y))
}

fn render_attachment_preview_message(frame: &mut Frame, area: Rect, message: &'static str) {
    frame.render_widget(
        Paragraph::new(message)
            .alignment(ratatui::layout::Alignment::Center)
            .style(Style::new().fg(FG_MUTED).bg(BG)),
        area,
    );
}

pub(crate) fn render_attachment_preview(
    frame: &mut Frame,
    item: &TaskListItem,
    attachment_id: &str,
    widgets: &mut WidgetState,
    inline_images: Option<&DetailInlineImageContext>,
) {
    let area = detail_body_area(frame.area());
    frame.render_widget(Clear, area);
    let Some(attachment) = item.attachments.iter().find(|attachment| {
        attachment.attachment_id == attachment_id
            && !attachment.deleted
            && attachment.has_blob
            && attachment.bytes_state == crate::attachments::AttachmentBytesState::Present
            && attachment.media_type.starts_with("image/")
            && matches!(
                (attachment.width, attachment.height),
                (Some(width), Some(height)) if width > 0 && height > 0
            )
    }) else {
        render_attachment_preview_message(frame, area, "attachment is unavailable");
        return;
    };
    let title = attachment
        .filename
        .as_deref()
        .or(attachment.alt_text.as_deref())
        .unwrap_or("Image preview");
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(ACCENT))
        .title(format!(" {title} "))
        .style(Style::new().bg(BG));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(inline_images) = inline_images else {
        render_attachment_preview_message(frame, inner, "preview unavailable");
        return;
    };
    if inline_images
        .unavailable_hashes
        .contains(&attachment.sha256)
    {
        render_attachment_preview_message(frame, inner, "preview unavailable");
        return;
    }
    let (width, height) = fitted_image_size(attachment, inner.width, inner.height);
    if width == 0 || height == 0 {
        return;
    }
    let x = inner
        .x
        .saturating_add(inner.width.saturating_sub(width) / 2);
    let y = inner
        .y
        .saturating_add(inner.height.saturating_sub(height) / 2);
    widgets
        .inline_image_placements
        .push(DetailInlineImagePlacement {
            attachment_id: attachment.attachment_id.clone(),
            source_hash: attachment.sha256.clone(),
            x,
            y,
            width,
            height,
        });
}

fn fitted_image_size(
    attachment: &AttachmentMetadataJson,
    max_width: u16,
    max_height: u16,
) -> (u16, u16) {
    const CELL_HEIGHT_TO_WIDTH_RATIO: f64 = 2.0;
    let (Some(pixel_width), Some(pixel_height)) = (attachment.width, attachment.height) else {
        return (max_width, max_height);
    };
    if pixel_width <= 0 || pixel_height <= 0 || max_width == 0 || max_height == 0 {
        return (0, 0);
    }
    let aspect = pixel_width as f64 / pixel_height as f64;
    let width_at_max_height =
        (max_height as f64 * aspect * CELL_HEIGHT_TO_WIDTH_RATIO).round() as u16;
    if width_at_max_height <= max_width {
        (width_at_max_height.max(1), max_height)
    } else {
        let height = ((max_width as f64 / aspect) / CELL_HEIGHT_TO_WIDTH_RATIO).round() as u16;
        (max_width, height.max(1).min(max_height))
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_detail_underlay(
    frame: &mut Frame,
    store: &TuiStore,
    widgets: &mut WidgetState,
    selected_task: Option<usize>,
    scroll: u16,
    inline_title_editor: Option<&TextInputView>,
    active_target: Option<&DetailTargetId>,
    hovered_target: Option<&DetailTargetId>,
    expanded_sections: &BTreeSet<DetailSection>,
    selection: Option<&DetailTextSelection>,
    inline_images: Option<&DetailInlineImageContext>,
    pending_attachments: &[crate::tui::attachment_controller::PendingAttachmentView],
    removed_epic_child: Option<&crate::tui::app::RemovedEpicChild>,
) {
    if let Some(task) = store.selected_task(selected_task) {
        let context = DetailRenderContext {
            terminal_area: frame.area(),
            scroll,
            detail_revision: store.tasks.revision(),
            inline_title_editor,
            active_target,
            hovered_target,
            expanded_sections,
            selection,
            inline_images,
            pending_attachments,
            removed_epic_child,
        };
        render_detail(frame, task, &context, widgets);
    }
}

#[cfg(test)]
mod tests;
