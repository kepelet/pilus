---
title: Add a UI control
description: Extend style.rs and the playground before using a new element.
tags:
  - wiki
  - guide
---
# Add a UI control

## Goal

Add a button, field, label, switch, or other native element without letting it drift between windows.

## Steps

1. **Check the playground.** [ui_playground.rs](../../crates/pilus/src/gui/ui_playground.rs) is the visual contract; if the element already exists, use its builder instead of writing new chrome.
2. **Add it to `style.rs`.** Implement the control once in [style.rs](../../crates/pilus/src/gui/style.rs) beside its siblings (`button`, `small_button`, `field`, `section_label`, `toggle`, `chevron`, `spinner`, `heat_cell`, `rank_row`, `tree_row`).
3. **Catalogue it.** Add a playground section that renders every state: default, hover, focused, disabled, and the compact size where relevant.
4. **Scale type.** Resolve sizes with `style::Fonts::resolve` or `style::scaled`; a fixed `text_size(px(...))` for chrome text is a bug even when the default matches. Geometry is 28 px regular, 24 px compact, `rounded_md` for controls.
5. **Use theme roles.** Colors come from `Theme`; literals belong only in [theme.rs](../../crates/pilus/src/gui/theme.rs) and [scheme.rs](../../crates/pilus/src/gui/scheme.rs). Every border is `theme.border`; a focused edge is `theme.border_focus`.
6. **Animate the state change.** Hover and selection go through `FillExt::hover_fill` / `state_fill` (call them before adding children). A floating panel opens through `style::enter`. Honor `cx.reduce_motion()`.
7. **Render a capture.** `PILUS_SHOT_FONT_SIZE=15 cargo run -p pilus --features visual-test -- --ui-shots` catches chrome that ignores the Interface setting.

## Relevant code

- [style.rs](../../crates/pilus/src/gui/style.rs), [ui_playground.rs](../../crates/pilus/src/gui/ui_playground.rs)
- [theme.rs](../../crates/pilus/src/gui/theme.rs), [scheme.rs](../../crates/pilus/src/gui/scheme.rs), [fonts.rs](../../crates/pilus/src/gui/fonts.rs)
- [apply_chrome_font](../../crates/pilus/src/gui/mod.rs) in `gui/mod.rs`

## Gotchas

- Do not hand-roll a button or field in a page; that is how two windows end up with different paddings and radii.
- `selection_bg`/`selection_fg` are for text selection and list highlighting, not sidebar rows; sidebar selection is the `theme.panel` pill.
- Accent means progress or focus, not a panel edge.
- A new always-on animation must count as “moving” in the tick loop or it stutters at the idle rate, and must check `style::energy()` if it costs GPU time.

## Related

- [Native chrome and GPUI](../architecture/native-chrome.md)
- [Idle cost](../concepts/idle-cost.md)
- [Run visual captures](./visual-captures.md)
