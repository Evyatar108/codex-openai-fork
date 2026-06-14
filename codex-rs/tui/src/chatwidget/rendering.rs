//! Render composition for the main chat widget surface.

use super::committed_transcript::RetainedTranscriptViewportRenderable;
use super::*;

impl ChatWidget {
    pub(super) fn as_renderable(&self) -> RenderableItem<'_> {
        let active_cell_right_reserve = self.ambient_pet_wrap_reserved_cols();
        let active_cell_renderable = match &self.transcript.active_cell {
            Some(cell) => RenderableItem::Owned(Box::new(TranscriptAreaRenderable {
                child: cell.as_ref(),
                top: 1,
                right: active_cell_right_reserve,
            })),
            None => RenderableItem::Owned(Box::new(())),
        };
        let active_hook_cell_renderable = match &self.active_hook_cell {
            Some(cell) if cell.should_render() => {
                RenderableItem::Owned(Box::new(TranscriptAreaRenderable {
                    child: cell,
                    top: 1,
                    right: active_cell_right_reserve,
                }))
            }
            _ => RenderableItem::Owned(Box::new(())),
        };
        let mut flex = FlexRenderable::new();
        flex.push(/*flex*/ 1, active_cell_renderable);
        flex.push(/*flex*/ 0, active_hook_cell_renderable);
        flex.push(
            /*flex*/ 0,
            RenderableItem::Owned(Box::new(BottomPaneComposerReserveRenderable {
                bottom_pane: &self.bottom_pane,
                right_reserve: active_cell_right_reserve,
            }))
            .inset(Insets::tlbr(
                /*top*/ 1, /*left*/ 0, /*bottom*/ 0, /*right*/ 0,
            )),
        );
        RenderableItem::Owned(Box::new(flex))
    }

    // SANDBOX PATCH: Feature-enabled main chat rendering now owns committed transcript cells inline.
    pub(crate) fn render_with_committed_cells(
        &self,
        committed_cells: &[Arc<dyn HistoryCell>],
        area: Rect,
        buf: &mut Buffer,
    ) {
        self.as_retained_renderable(committed_cells)
            .render(area, buf);
        self.last_rendered_width.set(Some(area.width as usize));
    }

    #[cfg(test)]
    pub(crate) fn set_active_cell_for_tests(&mut self, active_cell: Box<dyn HistoryCell>) {
        self.transcript.active_cell = Some(active_cell);
        self.bump_active_cell_revision();
    }

    pub(crate) fn desired_height_with_committed_cells(
        &self,
        committed_cells: &[Arc<dyn HistoryCell>],
        width: u16,
        max_height: u16,
    ) -> u16 {
        let active_cell_right_reserve = self.ambient_pet_wrap_reserved_cols();
        let retained_transcript = RetainedTranscriptViewportRenderable::new(
            committed_cells,
            self.transcript.active_cell.as_deref(),
            active_cell_right_reserve,
            self.history_render_mode(),
        );
        let transcript_budget = max_height.saturating_sub(
            self.active_hook_renderable(active_cell_right_reserve)
                .desired_height(width)
                .saturating_add(
                    BottomPaneComposerReserveRenderable {
                        bottom_pane: &self.bottom_pane,
                        right_reserve: active_cell_right_reserve,
                    }
                    .inset(Insets::tlbr(
                        /*top*/ 1, /*left*/ 0, /*bottom*/ 0, /*right*/ 0,
                    ))
                    .desired_height(width),
                ),
        );
        retained_transcript
            .visible_height(width, transcript_budget)
            .saturating_add(
                self.active_hook_renderable(active_cell_right_reserve)
                    .desired_height(width),
            )
            .saturating_add(
                BottomPaneComposerReserveRenderable {
                    bottom_pane: &self.bottom_pane,
                    right_reserve: active_cell_right_reserve,
                }
                .inset(Insets::tlbr(
                    /*top*/ 1, /*left*/ 0, /*bottom*/ 0, /*right*/ 0,
                ))
                .desired_height(width),
            )
            .min(max_height)
    }

    pub(crate) fn cursor_pos_with_committed_cells(
        &self,
        committed_cells: &[Arc<dyn HistoryCell>],
        area: Rect,
    ) -> Option<(u16, u16)> {
        self.as_retained_renderable(committed_cells)
            .cursor_pos(area)
    }

    pub(crate) fn cursor_style_with_committed_cells(
        &self,
        committed_cells: &[Arc<dyn HistoryCell>],
        area: Rect,
    ) -> crossterm::cursor::SetCursorStyle {
        self.as_retained_renderable(committed_cells)
            .cursor_style(area)
    }

    fn as_retained_renderable<'a>(
        &'a self,
        committed_cells: &'a [Arc<dyn HistoryCell>],
    ) -> RenderableItem<'a> {
        let active_cell_right_reserve = self.ambient_pet_wrap_reserved_cols();
        let mut flex = FlexRenderable::new();
        flex.push(
            /*flex*/ 1,
            RenderableItem::Owned(Box::new(RetainedTranscriptViewportRenderable::new(
                committed_cells,
                self.transcript.active_cell.as_deref(),
                active_cell_right_reserve,
                self.history_render_mode(),
            ))),
        );
        flex.push(
            /*flex*/ 0,
            self.active_hook_renderable(active_cell_right_reserve),
        );
        flex.push(
            /*flex*/ 0,
            RenderableItem::Owned(Box::new(BottomPaneComposerReserveRenderable {
                bottom_pane: &self.bottom_pane,
                right_reserve: active_cell_right_reserve,
            }))
            .inset(Insets::tlbr(
                /*top*/ 1, /*left*/ 0, /*bottom*/ 0, /*right*/ 0,
            )),
        );
        RenderableItem::Owned(Box::new(flex))
    }

    fn active_hook_renderable(&self, active_cell_right_reserve: u16) -> RenderableItem<'_> {
        match &self.active_hook_cell {
            Some(cell) if cell.should_render() => {
                RenderableItem::Owned(Box::new(TranscriptAreaRenderable {
                    child: cell,
                    top: 1,
                    right: active_cell_right_reserve,
                }))
            }
            _ => RenderableItem::Owned(Box::new(())),
        }
    }
}

struct BottomPaneComposerReserveRenderable<'a> {
    bottom_pane: &'a BottomPane,
    right_reserve: u16,
}

impl Renderable for BottomPaneComposerReserveRenderable<'_> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.bottom_pane
            .render_with_composer_right_reserve(area, buf, self.right_reserve);
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.bottom_pane
            .desired_height_with_composer_right_reserve(width, self.right_reserve)
    }

    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        self.bottom_pane
            .cursor_pos_with_composer_right_reserve(area, self.right_reserve)
    }

    fn cursor_style(&self, area: Rect) -> crossterm::cursor::SetCursorStyle {
        self.bottom_pane
            .cursor_style_with_composer_right_reserve(area, self.right_reserve)
    }
}

struct TranscriptAreaRenderable<'a> {
    child: &'a dyn HistoryCell,
    top: u16,
    right: u16,
}

impl Renderable for TranscriptAreaRenderable<'_> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let area = self.child_area(area);
        let lines = self.child.display_lines(area.width);
        let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
        let y = if area.height == 0 {
            0
        } else {
            let overflow = paragraph
                .line_count(area.width)
                .saturating_sub(usize::from(area.height));
            u16::try_from(overflow).unwrap_or(u16::MAX)
        };
        Clear.render(area, buf);
        paragraph.scroll((y, 0)).render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        let child_width = width.saturating_sub(self.right).max(1);
        HistoryCell::desired_height(self.child, child_width) + self.top
    }
}

impl TranscriptAreaRenderable<'_> {
    fn child_area(&self, area: Rect) -> Rect {
        let y = area.y.saturating_add(self.top);
        let height = area.height.saturating_sub(self.top);
        Rect::new(
            area.x,
            y,
            area.width.saturating_sub(self.right).max(1),
            height,
        )
    }
}

impl Renderable for ChatWidget {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.as_renderable().render(area, buf);
        self.last_rendered_width.set(Some(area.width as usize));
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.as_renderable().desired_height(width)
    }

    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        self.as_renderable().cursor_pos(area)
    }

    fn cursor_style(&self, area: Rect) -> crossterm::cursor::SetCursorStyle {
        self.as_renderable().cursor_style(area)
    }
}
