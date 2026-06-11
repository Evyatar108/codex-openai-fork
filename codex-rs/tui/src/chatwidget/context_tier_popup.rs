//! Knob B context-window tier picker for `ChatWidget`.
//!
//! SANDBOX PATCH: Knob B context-window tier. This is the stage-3 picker shown
//! after the model (stage 1) and reasoning-effort (stage 2) pickers, for
//! Copilot-provider models that expose a curated default window below their
//! full ceiling (`ModelPreset::supports_context_window_tier_selection`). The
//! picker is skipped entirely for single-tier models. See
//! docs/implementation/patch-surface.md §14.

use super::*;

use codex_protocol::openai_models::ContextWindowTier;

impl ChatWidget {
    /// Open the context-window tier picker for `model`, after `effort` has been
    /// settled. Selecting a tier applies + persists the full model/effort/tier
    /// selection. If the model is not two-tier (defensive — the convergence
    /// seam should not call this for single-tier models), fall back to
    /// persisting the model/effort selection directly.
    pub(crate) fn open_context_tier_popup(
        &mut self,
        model: String,
        effort: Option<ReasoningEffortConfig>,
    ) {
        let preset = self
            .model_catalog
            .try_list_models()
            .ok()
            .and_then(|presets| presets.into_iter().find(|preset| preset.model == model));

        let Some(preset) = preset.filter(ModelPreset::supports_context_window_tier_selection)
        else {
            // Single-tier (or unknown) model: nothing to choose. Persist the
            // model/effort selection directly so the flow still completes.
            self.app_event_tx
                .send(AppEvent::PersistModelSelection { model, effort });
            return;
        };

        let default_window = preset.context_window_for_tier(ContextWindowTier::Default);
        let full_window = preset.context_window_for_tier(ContextWindowTier::LongContext);
        let current_tier = self.config.model_context_tier.unwrap_or_default();

        let mut items: Vec<SelectionItem> = Vec::new();
        for (tier, window, base_label) in [
            (ContextWindowTier::Default, default_window, "Default"),
            (ContextWindowTier::LongContext, full_window, "Long context"),
        ] {
            let mut label = base_label.to_string();
            if tier == ContextWindowTier::Default {
                label.push_str(" (default)");
            }
            let description = window
                .map(|tokens| format!("{} token context window", format_token_window(tokens)));

            let model_for_action = model.clone();
            let actions: Vec<SelectionAction> = vec![Box::new(move |tx| {
                tx.send(AppEvent::UpdateContextTier(Some(tier)));
                tx.send(AppEvent::PersistModelSelection {
                    model: model_for_action.clone(),
                    effort,
                });
                tx.send(AppEvent::PersistContextTier(Some(tier)));
            })];

            items.push(SelectionItem {
                name: label,
                description,
                is_current: tier == current_tier,
                actions,
                dismiss_on_select: true,
                ..Default::default()
            });
        }

        let mut header = ColumnRenderable::new();
        header.push(Line::from(
            format!("Select Context Window for {model}").bold(),
        ));

        self.bottom_pane.show_selection_view(SelectionViewParams {
            header: Box::new(header),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            ..Default::default()
        });
    }
}

/// Format a token count with thousands separators (e.g. `1050000` -> `1,050,000`).
fn format_token_window(tokens: i64) -> String {
    let digits = tokens.unsigned_abs().to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    let len = digits.len();
    for (idx, ch) in digits.chars().enumerate() {
        if idx > 0 && (len - idx) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    if tokens < 0 {
        format!("-{grouped}")
    } else {
        grouped
    }
}

#[cfg(test)]
mod tests {
    use super::format_token_window;

    #[test]
    fn formats_token_windows_with_separators() {
        assert_eq!(format_token_window(1_050_000), "1,050,000");
        assert_eq!(format_token_window(400_000), "400,000");
        assert_eq!(format_token_window(264_000), "264,000");
        assert_eq!(format_token_window(999), "999");
    }
}
