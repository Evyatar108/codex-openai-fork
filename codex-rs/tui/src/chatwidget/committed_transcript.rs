use std::sync::Arc;

use crate::history_cell::HistoryCell;
use crate::history_cell::HistoryRenderMode;
use crate::history_cell::UserHistoryCell;
use crate::render::renderable::Renderable;
use crate::style::user_message_style;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Text;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use ratatui::widgets::Wrap;

// SANDBOX PATCH: Shared retained committed-transcript helpers for the main viewport and Ctrl+T.
pub(crate) fn render_committed_cells(
    cells: &[Arc<dyn HistoryCell>],
    highlight_cell: Option<usize>,
) -> Vec<Box<dyn Renderable>> {
    cells
        .iter()
        .enumerate()
        .map(|(index, cell)| {
            Box::new(CachedRenderable::new(CommittedCellRenderable {
                cell: cell.clone(),
                separator_before: committed_cell_has_separator_before(index, cell.as_ref()),
                style: committed_cell_style(cell.as_ref(), highlight_cell == Some(index)),
            })) as Box<dyn Renderable>
        })
        .collect()
}

pub(crate) fn committed_cell_has_separator_before(index: usize, cell: &dyn HistoryCell) -> bool {
    index > 0 && !cell.is_stream_continuation()
}

pub(crate) fn committed_cell_total_height(index: usize, cell: &dyn HistoryCell, width: u16) -> u16 {
    cell.desired_transcript_height(width)
        .saturating_add(u16::from(committed_cell_has_separator_before(index, cell)))
}

fn committed_cell_style(cell: &dyn HistoryCell, highlighted: bool) -> Style {
    if cell.as_any().is::<UserHistoryCell>() {
        if highlighted {
            user_message_style().reversed()
        } else {
            user_message_style()
        }
    } else {
        Style::default()
    }
}

fn committed_cell_lines(
    cell: &dyn HistoryCell,
    width: u16,
    separator_before: bool,
    style: Style,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if separator_before {
        lines.push(Line::from(""));
    }
    lines.extend(
        cell.transcript_lines(width)
            .into_iter()
            .map(|line| line.patch_style(style)),
    );
    lines
}

fn active_cell_lines(
    active_cell: &dyn HistoryCell,
    width: u16,
    render_mode: HistoryRenderMode,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from("")];
    lines.extend(active_cell.display_lines_for_mode(width, render_mode));
    lines
}

// SANDBOX PATCH: The main viewport now renders only the visible committed tail from retained cells.
pub(crate) struct RetainedTranscriptViewportRenderable<'a> {
    committed_cells: &'a [Arc<dyn HistoryCell>],
    active_cell: Option<&'a dyn HistoryCell>,
    right_reserve: u16,
    render_mode: HistoryRenderMode,
}

impl<'a> RetainedTranscriptViewportRenderable<'a> {
    pub(crate) fn new(
        committed_cells: &'a [Arc<dyn HistoryCell>],
        active_cell: Option<&'a dyn HistoryCell>,
        right_reserve: u16,
        render_mode: HistoryRenderMode,
    ) -> Self {
        Self {
            committed_cells,
            active_cell,
            right_reserve,
            render_mode,
        }
    }

    pub(crate) fn visible_height(&self, width: u16, max_height: u16) -> u16 {
        let content_width = self.content_width(width);
        let mut total_height = self.active_total_height(content_width);
        if total_height >= max_height {
            return max_height;
        }

        for (index, cell) in self.committed_cells.iter().enumerate().rev() {
            total_height = total_height.saturating_add(committed_cell_total_height(
                index,
                cell.as_ref(),
                content_width,
            ));
            if total_height >= max_height {
                return max_height;
            }
        }

        total_height
    }

    fn content_width(&self, width: u16) -> u16 {
        width.saturating_sub(self.right_reserve).max(1)
    }

    fn active_total_height(&self, width: u16) -> u16 {
        self.active_cell.map_or(0, |cell| {
            cell.desired_height_for_mode(width, self.render_mode)
                .saturating_add(1)
        })
    }

    fn render_active_tail(&self, area: Rect, buf: &mut Buffer) {
        let Some(active_cell) = self.active_cell else {
            return;
        };

        let lines = active_cell_lines(active_cell, area.width, self.render_mode);
        let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
        let overflow = paragraph
            .line_count(area.width)
            .saturating_sub(usize::from(area.height));
        let scroll = u16::try_from(overflow).unwrap_or(u16::MAX);
        paragraph.scroll((scroll, 0)).render(area, buf);
    }

    fn render_committed_tail(&self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }

        let mut visible_cells: Vec<(usize, &Arc<dyn HistoryCell>, u16)> = Vec::new();
        let mut remaining_rows = area.height;

        for (index, cell) in self.committed_cells.iter().enumerate().rev() {
            let cell_height = committed_cell_total_height(index, cell.as_ref(), area.width);
            if cell_height == 0 {
                continue;
            }

            if cell_height >= remaining_rows {
                let top_clip = cell_height - remaining_rows;
                visible_cells.push((index, cell, top_clip));
                break;
            }

            visible_cells.push((index, cell, 0));
            remaining_rows -= cell_height;
            if remaining_rows == 0 {
                break;
            }
        }

        visible_cells.reverse();

        let mut y = area.y;
        for (index, cell, top_clip) in visible_cells {
            let separator_before = committed_cell_has_separator_before(index, cell.as_ref());
            let lines = committed_cell_lines(
                cell.as_ref(),
                area.width,
                separator_before,
                committed_cell_style(cell.as_ref(), /*highlighted*/ false),
            );
            let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
            let total_height = committed_cell_total_height(index, cell.as_ref(), area.width);
            let visible_height = total_height.saturating_sub(top_clip);
            let remaining_height = area.bottom().saturating_sub(y);
            let render_height = visible_height.min(remaining_height);
            if render_height == 0 {
                continue;
            }

            let render_area = Rect::new(area.x, y, area.width, render_height);
            paragraph.scroll((top_clip, 0)).render(render_area, buf);
            y = y.saturating_add(render_height);
            if y >= area.bottom() {
                break;
            }
        }
    }
}

impl Renderable for RetainedTranscriptViewportRenderable<'_> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let content_area = Rect::new(area.x, area.y, self.content_width(area.width), area.height);
        if content_area.height == 0 {
            return;
        }

        Clear.render(content_area, buf);

        let active_height = self.active_total_height(content_area.width);
        let active_area_height = active_height.min(content_area.height);
        let committed_area_height = content_area.height.saturating_sub(active_area_height);

        if committed_area_height > 0 {
            self.render_committed_tail(
                Rect::new(
                    content_area.x,
                    content_area.y,
                    content_area.width,
                    committed_area_height,
                ),
                buf,
            );
        }

        if active_area_height > 0 {
            self.render_active_tail(
                Rect::new(
                    content_area.x,
                    content_area.bottom().saturating_sub(active_area_height),
                    content_area.width,
                    active_area_height,
                ),
                buf,
            );
        }
    }

    fn desired_height(&self, width: u16) -> u16 {
        let content_width = self.content_width(width);
        let mut total_height = self.active_total_height(content_width);
        for (index, cell) in self.committed_cells.iter().enumerate() {
            total_height = total_height.saturating_add(committed_cell_total_height(
                index,
                cell.as_ref(),
                content_width,
            ));
        }
        total_height
    }
}

pub(crate) struct CachedRenderable {
    renderable: Box<dyn Renderable>,
    height: std::cell::Cell<Option<u16>>,
    last_width: std::cell::Cell<Option<u16>>,
}

impl CachedRenderable {
    pub(crate) fn new(renderable: impl Into<Box<dyn Renderable>>) -> Self {
        Self {
            renderable: renderable.into(),
            height: std::cell::Cell::new(None),
            last_width: std::cell::Cell::new(None),
        }
    }
}

impl Renderable for CachedRenderable {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.renderable.render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        if self.last_width.get() != Some(width) {
            let height = self.renderable.desired_height(width);
            self.height.set(Some(height));
            self.last_width.set(Some(width));
        }
        self.height.get().unwrap_or(0)
    }
}

pub(crate) struct CommittedCellRenderable {
    cell: Arc<dyn HistoryCell>,
    separator_before: bool,
    style: Style,
}

impl Renderable for CommittedCellRenderable {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        Paragraph::new(Text::from(committed_cell_lines(
            self.cell.as_ref(),
            area.width,
            self.separator_before,
            self.style,
        )))
        .wrap(Wrap { trim: false })
        .render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.cell
            .desired_transcript_height(width)
            .saturating_add(u16::from(self.separator_before))
    }
}
