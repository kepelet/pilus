//! The three-dot menu at a page's top right, where Refresh, Sources and Close
//! live instead of a row of buttons.
//!
//! The page owns one `bool` for whether the list is dropped (reset when the
//! page closes) and hands this module an accessor for it, as the machine
//! pickers do, so the menu itself holds no state.

use gpui::{AnyElement, Context, anchored, deferred, div, point, prelude::*, px};

use super::style::{self, ButtonKind};
use super::{PICKER_MENU_MAX_H, PilusView, picker_menu, picker_row};

/// Width of the dropped list, before the Interface size scales it.
const MENU_W: f32 = 150.0;

/// One row of a page menu.
pub(super) struct PageAction {
    pub(super) label: &'static str,
    pub(super) run: fn(&mut PilusView, &mut Context<PilusView>),
}

impl PilusView {
    /// The trigger and, while `open`, its list under it, right-aligned so the
    /// list opens inward from the card's edge. `flag` reaches the page's
    /// "dropped" bool.
    pub(super) fn page_menu(
        &self,
        id: &'static str,
        open: bool,
        actions: Vec<PageAction>,
        flag: fn(&mut PilusView) -> &mut bool,
        cx: &mut Context<PilusView>,
    ) -> AnyElement {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let side = style::scaled(&self.shared_settings, style::CONTROL_HEIGHT_SMALL);
        let mut trigger = style::icon_button(
            format!("pilus-{id}-menu"),
            include_bytes!("../../assets/icons/ellipsis-vertical.svg"),
            ButtonKind::Ghost,
            theme,
            side,
        )
        .relative()
        .on_click(cx.listener(move |this, _, _, cx| {
            let open = flag(this);
            *open = !*open;
            cx.notify();
        }));
        if open {
            let mut list = picker_menu(format!("pilus-{id}-menu-list"), theme)
                .w(px(style::scaled(&self.shared_settings, MENU_W)))
                .max_h(px(PICKER_MENU_MAX_H))
                .on_mouse_down_out(cx.listener(move |this, _, _, cx| {
                    *flag(this) = false;
                    cx.notify();
                }));
            for (index, action) in actions.into_iter().enumerate() {
                let run = action.run;
                list = list.child(picker_row(
                    format!("pilus-{id}-menu-{index}"),
                    action.label.to_string(),
                    false,
                    theme,
                    fonts.body,
                    cx.listener(move |this, _, _, cx| {
                        *flag(this) = false;
                        run(this, cx);
                        cx.notify();
                    }),
                ));
            }
            trigger = trigger.child(
                div().absolute().top(px(0.0)).right(px(0.0)).child(
                    deferred(
                        anchored()
                            .anchor(gpui::Anchor::TopRight)
                            .offset(point(px(0.0), px(side + 4.0)))
                            .snap_to_window_with_margin(px(8.0))
                            .child(style::enter(format!("pilus-{id}-menu-enter"), list)),
                    )
                    .with_priority(2),
                ),
            );
        }
        trigger.into_any_element()
    }
}
