//! Shared chrome tokens and the control builders every surface draws from.
//!
//! The UI Playground is the visual contract for pilus chrome; these builders are
//! the single implementation of it, so a button or field cannot drift between
//! windows. Sizes here are design sizes at the default Interface size and are
//! scaled to the user's setting by [`scaled`].

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{Div, ElementId, Font, FontWeight, IntoElement, Stateful, div, font, prelude::*, px};

use crate::settings::{CHROME_FONT, Settings};

use super::theme::Theme;

/// Base chrome text: body copy, rows, and editable control values.
pub(super) const TEXT_BODY: f32 = 13.0;
/// Secondary text: footer, row detail, and feedback messages.
pub(super) const TEXT_SECONDARY: f32 = 12.0;
/// Section labels and small action text.
pub(super) const TEXT_LABEL: f32 = 11.0;
/// Regular control height: buttons and field boxes.
pub(super) const CONTROL_HEIGHT: f32 = 28.0;
/// Compact control height: dense rows and section-header actions.
pub(super) const CONTROL_HEIGHT_SMALL: f32 = 24.0;

/// Scale a design-space chrome size to the user's Interface size.
pub(super) fn scaled(settings: &Rc<RefCell<Settings>>, design: f32) -> f32 {
    (settings.borrow().ui_font_size + (design - CHROME_FONT)).max(8.0)
}

/// The type scale a window renders with, resolved once per frame.
#[derive(Clone, Copy)]
pub(super) struct Fonts {
    /// Body text.
    pub(super) body: f32,
    /// Secondary text, one step below the body.
    pub(super) secondary: f32,
    /// Section labels and small action text, two steps below the body.
    pub(super) label: f32,
}

impl Fonts {
    /// Resolve the scale from the shared settings.
    pub(super) fn resolve(settings: &Rc<RefCell<Settings>>) -> Self {
        Self {
            body: scaled(settings, TEXT_BODY),
            secondary: scaled(settings, TEXT_SECONDARY),
            label: scaled(settings, TEXT_LABEL),
        }
    }
}

/// The chrome font family, or the platform default when the setting is unset.
///
/// Text drawn through GPUI's shaper needs the family explicitly; a window's
/// `font_family` style does not reach [`gpui::ShapedLine`].
pub(super) fn chrome_font(settings: &Rc<RefCell<Settings>>) -> Font {
    match settings.borrow().ui_font.as_ref() {
        Some(family) => font(family.clone()),
        None => Font::default(),
    }
}

/// A section label: small, bold, and muted. Labels are uppercase by convention.
pub(super) fn section_label(theme: Theme, text: impl IntoElement, size: f32) -> Div {
    div()
        .text_size(px(size))
        .font_weight(FontWeight::BOLD)
        .text_color(theme.muted)
        .child(text)
}

/// The catalogue's button treatments.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ButtonKind {
    /// Accent-filled and bold: the one affirmative action in a surface.
    Primary,
    /// Panel-dim with a border: an alternate action next to a primary.
    Secondary,
    /// Transparent and muted: cancel, remove, and other quiet actions.
    Ghost,
    /// Panel-dim with alert text: destructive.
    Danger,
}

/// A regular-height chrome button. Callers attach their own handler.
pub(super) fn button(
    id: impl Into<ElementId>,
    label: impl IntoElement,
    kind: ButtonKind,
    theme: Theme,
    text_size: f32,
) -> Stateful<Div> {
    button_at(id, label, kind, theme, text_size, CONTROL_HEIGHT, px(12.0))
}

/// A compact-height chrome button for dense rows and section headers.
pub(super) fn small_button(
    id: impl Into<ElementId>,
    label: impl IntoElement,
    kind: ButtonKind,
    theme: Theme,
    text_size: f32,
) -> Stateful<Div> {
    button_at(
        id,
        label,
        kind,
        theme,
        text_size,
        CONTROL_HEIGHT_SMALL,
        px(8.0),
    )
}

/// One button at an explicit height and horizontal padding.
fn button_at(
    id: impl Into<ElementId>,
    label: impl IntoElement,
    kind: ButtonKind,
    theme: Theme,
    text_size: f32,
    height: f32,
    padding: gpui::Pixels,
) -> Stateful<Div> {
    let (bg, fg, border) = button_colors(kind, theme);
    let hover_bg = match kind {
        ButtonKind::Ghost => theme.panel_dim,
        _ => theme.panel,
    };
    let hover_fg = match kind {
        ButtonKind::Danger => theme.alert,
        _ => theme.panel_fg,
    };
    let bold = matches!(kind, ButtonKind::Primary);
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .gap_2()
        .h(px(height))
        .px(padding)
        .rounded_md()
        .bg(bg)
        .text_color(fg)
        .border_1()
        .border_color(border)
        .text_size(px(text_size))
        .font_weight(if bold {
            FontWeight::BOLD
        } else {
            FontWeight::NORMAL
        })
        .cursor_pointer()
        .hover(move |style| style.bg(hover_bg).text_color(hover_fg))
        .child(label)
}

/// The background, foreground, and border triple for a button treatment.
fn button_colors(kind: ButtonKind, theme: Theme) -> (gpui::Rgba, gpui::Rgba, gpui::Rgba) {
    match kind {
        ButtonKind::Primary => (theme.accent, theme.window_bg, theme.accent),
        ButtonKind::Secondary => (theme.panel_dim, theme.fg, theme.border),
        ButtonKind::Ghost => (
            theme.panel.opacity(0.0),
            theme.muted,
            theme.panel.opacity(0.0),
        ),
        ButtonKind::Danger => (theme.panel_dim, theme.alert, theme.border),
    }
}

/// One labelled field: a small muted label above a boxed control.
///
/// The caller supplies the editable body and owns focus and click behavior.
/// `height` is a minimum, so a multi-line body can grow.
pub(super) fn field(
    id: impl Into<ElementId>,
    label: &str,
    focused: bool,
    theme: Theme,
    height: f32,
    label_size: f32,
    body: impl IntoElement,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(px(label_size))
                .text_color(theme.muted)
                .child(label.to_string()),
        )
        .child(
            div()
                .flex()
                .items_center()
                .min_h(px(height))
                .px_2()
                .rounded_md()
                .bg(theme.panel_dim)
                .border_1()
                .border_color(if focused { theme.accent } else { theme.border })
                .cursor_pointer()
                .child(body),
        )
}
