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

// SANDBOX PATCH: shared retained viewport area allocation for render and visible-height prepass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RetainedTranscriptAllocation {
    committed_area_height: u16,
    active_area_height: u16,
}

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

fn line_has_readable_text(line: &Line<'_>) -> bool {
    line.spans
        .iter()
        .any(|span| !span.content.trim().is_empty())
}

fn line_height(line: Line<'static>, width: u16) -> u16 {
    let row_count = Paragraph::new(Text::from(vec![line]))
        .wrap(Wrap { trim: false })
        .line_count(width);
    u16::try_from(row_count).unwrap_or(u16::MAX)
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
        // SANDBOX PATCH: match render() allocation so resize prepass and draw agree.
        let allocation = self.allocate_viewport(content_width, max_height);
        let committed_visible_height =
            self.visible_committed_height(content_width, allocation.committed_area_height);
        allocation
            .active_area_height
            .saturating_add(committed_visible_height)
            .min(max_height)
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

    fn allocate_viewport(&self, width: u16, max_height: u16) -> RetainedTranscriptAllocation {
        let active_height = self.active_total_height(width);
        // SANDBOX PATCH: reserve readable recent committed context before a tall active tail.
        let committed_reservation_height =
            self.recent_committed_context_reservation_height(width, max_height);
        // SANDBOX PATCH: leave the active tail at least one row whenever both regions can fit.
        let active_area_height =
            active_height.min(max_height.saturating_sub(committed_reservation_height));
        // SANDBOX PATCH: any rows not needed by the active tail remain available to committed cells.
        let committed_area_height = max_height.saturating_sub(active_area_height);

        RetainedTranscriptAllocation {
            committed_area_height,
            active_area_height,
        }
    }

    fn recent_committed_context_reservation_height(&self, width: u16, max_height: u16) -> u16 {
        if self.active_cell.is_none() || self.committed_cells.is_empty() || max_height <= 1 {
            return 0;
        }

        let Some((index, cell)) = self.committed_cells.iter().enumerate().next_back() else {
            return 0;
        };
        let readable_suffix_height =
            committed_cell_readable_suffix_height(index, cell.as_ref(), width);

        readable_suffix_height.min(max_height.saturating_sub(1))
    }

    fn visible_committed_height(&self, width: u16, max_height: u16) -> u16 {
        let mut total_height = 0u16;
        for (index, cell) in self.committed_cells.iter().enumerate().rev() {
            total_height = total_height.saturating_add(committed_cell_total_height(
                index,
                cell.as_ref(),
                width,
            ));
            if total_height >= max_height {
                return max_height;
            }
        }

        total_height
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

fn committed_cell_readable_suffix_height(index: usize, cell: &dyn HistoryCell, width: u16) -> u16 {
    let mut suffix_height = 0u16;
    for line in committed_cell_lines(
        cell,
        width,
        committed_cell_has_separator_before(index, cell),
        committed_cell_style(cell, /*highlighted*/ false),
    )
    .into_iter()
    .rev()
    {
        let has_readable_text = line_has_readable_text(&line);
        suffix_height = suffix_height.saturating_add(line_height(line, width));
        if has_readable_text {
            return suffix_height;
        }
    }

    0
}

impl Renderable for RetainedTranscriptViewportRenderable<'_> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let content_area = Rect::new(area.x, area.y, self.content_width(area.width), area.height);
        if content_area.height == 0 {
            return;
        }

        Clear.render(content_area, buf);

        let allocation = self.allocate_viewport(content_area.width, content_area.height);

        if allocation.committed_area_height > 0 {
            self.render_committed_tail(
                Rect::new(
                    content_area.x,
                    content_area.y,
                    content_area.width,
                    allocation.committed_area_height,
                ),
                buf,
            );
        }

        if allocation.active_area_height > 0 {
            self.render_active_tail(
                Rect::new(
                    content_area.x,
                    content_area
                        .bottom()
                        .saturating_sub(allocation.active_area_height),
                    content_area.width,
                    allocation.active_area_height,
                ),
                buf,
            );
        }
    }

    fn desired_height(&self, _width: u16) -> u16 {
        if self.active_cell.is_none() && self.committed_cells.is_empty() {
            0
        } else {
            u16::MAX
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    #[derive(Debug)]
    struct MeasuringHistoryCell {
        desired_transcript_height_calls: Arc<AtomicUsize>,
    }

    impl HistoryCell for MeasuringHistoryCell {
        fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
            vec!["not measured".into()]
        }

        fn raw_lines(&self) -> Vec<Line<'static>> {
            self.display_lines(/*width*/ 80)
        }

        fn desired_transcript_height(&self, _width: u16) -> u16 {
            self.desired_transcript_height_calls
                .fetch_add(1, Ordering::SeqCst);
            1
        }
    }

    #[test]
    fn retained_desired_height_claims_available_space_without_measuring_committed_cells() {
        let counter = Arc::new(AtomicUsize::new(0));
        let committed_cells: Vec<Arc<dyn HistoryCell>> = (0..1_000)
            .map(|_| {
                Arc::new(MeasuringHistoryCell {
                    desired_transcript_height_calls: Arc::clone(&counter),
                }) as Arc<dyn HistoryCell>
            })
            .collect();
        let renderable = RetainedTranscriptViewportRenderable::new(
            &committed_cells,
            None,
            /*right_reserve*/ 0,
            HistoryRenderMode::Rich,
        );

        assert_eq!(renderable.desired_height(/*width*/ 80), u16::MAX);
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn retained_desired_height_is_zero_when_empty() {
        let committed_cells = Vec::new();
        let renderable = RetainedTranscriptViewportRenderable::new(
            &committed_cells,
            None,
            /*right_reserve*/ 0,
            HistoryRenderMode::Rich,
        );

        assert_eq!(renderable.desired_height(/*width*/ 80), 0);
    }
}
