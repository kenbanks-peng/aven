use ratatui::layout::{Constraint, Layout, Rect};

use crate::config::TableColumn;

/// Frame-local content geometry indexed by semantic identity, not display position.
#[derive(Debug, Clone, Copy)]
pub(super) struct TableLayout {
    cells: [Rect; 8],
}

impl TableLayout {
    pub(super) fn resolve(columns: &[Constraint; 8], order: &[TableColumn], width: u16) -> Self {
        // Fixed widths include their default gutters; content width stays semantic.
        let constraints = order.iter().enumerate().map(|(position, column)| {
            let constraint = columns[*column as usize];
            match constraint {
                Constraint::Length(width) if width > 0 => {
                    let content_width = if *column == TableColumn::Time {
                        width
                    } else {
                        width.saturating_sub(1)
                    };
                    Constraint::Length(content_width + u16::from(position + 1 < order.len()))
                }
                other => other,
            }
        });
        let areas = Layout::horizontal(constraints).split(Rect::new(0, 0, width, 1));
        let mut cells = [Rect::default(); 8];
        for (position, (column, area)) in order.iter().zip(areas.iter()).enumerate() {
            let mut area = *area;
            if position + 1 < order.len() {
                area.width = area.width.saturating_sub(1);
            }
            cells[*column as usize] = area;
        }
        Self { cells }
    }

    pub(super) fn cell(self, column: TableColumn, row: Rect) -> Rect {
        let area = self.cells[column as usize];
        Rect::new(row.x.saturating_add(area.x), row.y, area.width, row.height)
    }

    pub(super) fn widths(self) -> [usize; 8] {
        self.cells.map(|area| area.width as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reordering_preserves_fixed_content_widths_and_collapsed_columns() {
        let columns = [
            Constraint::Length(12),
            Constraint::Fill(1),
            Constraint::Length(0),
            Constraint::Length(3),
            Constraint::Length(9),
            Constraint::Length(10),
            Constraint::Length(0),
            Constraint::Length(5),
        ];
        let original = TableLayout::resolve(&columns, &TableColumn::ALL, 120).widths();
        for rotation in 0..8 {
            let mut order = TableColumn::ALL;
            order.rotate_left(rotation);
            let widths = TableLayout::resolve(&columns, &order, 120).widths();
            for column in TableColumn::ALL {
                if column != TableColumn::Title {
                    assert_eq!(widths[column as usize], original[column as usize]);
                }
            }
        }
    }

    #[test]
    fn default_geometry_preserves_expansion_and_gutters_at_all_widths() {
        let columns = [
            Constraint::Length(12),
            Constraint::Fill(1),
            Constraint::Length(0),
            Constraint::Length(3),
            Constraint::Length(9),
            Constraint::Length(10),
            Constraint::Length(0),
            Constraint::Length(5),
        ];
        for width in [0, 1, 20, 40, 64, 89, 90, 120, 200] {
            let row = Rect::new(7, 3, width, 1);
            let original = Layout::horizontal(columns).areas::<8>(row);
            let layout = TableLayout::resolve(&columns, &TableColumn::ALL, width);
            for (index, column) in TableColumn::ALL.into_iter().enumerate() {
                let mut expected = original[index];
                if index < 7 {
                    expected.width = expected.width.saturating_sub(1);
                }
                assert_eq!(layout.cell(column, row), expected);
            }
        }
    }
}
