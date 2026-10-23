# UI

Read this before you draw anything, and render the result before you ship it.

## The contract

`crates/pilus/src/gui/ui_playground.rs` is the visual contract: a working
catalogue of every native element the app draws. `crates/pilus/src/gui/style.rs`
is the only implementation of it.

- Draw controls through `style::button`, `style::small_button`, `style::field`
  and `style::section_label`. Do not hand-roll a button, field or section label
  in another window.
- When an element is missing, add it to `style.rs` and to the playground first,
  then use it.
- There is one named exception. The sipil chat page's input is a flat line with
  no Send button and is deliberately not a `style::field`, so it reads like an
  IRC client (see [sipil/chat-page.md](sipil/chat-page.md)).

## Type, geometry, color

- **Type scale**: 13 body, 12 secondary, 11 label. Section labels are bold,
  muted and uppercase. Resolve sizes with `style::Fonts::resolve` or
  `style::scaled` so every surface follows the Interface size setting. A fixed
  `text_size(px(...))` on chrome text is a bug, even where the default size
  happens to match.
- **Geometry**: regular controls are 28px and compact ones 24px, with
  `px_3`/`px_2` padding. Use `rounded_md` for controls, cards and overlays, and
  `rounded_lg` for large floating panels. Draw radii and opacity through
  `Settings::drawn_radius` and `drawn_opacity`, never `radius` and
  `glass_opacity` (see [idle-cost.md](idle-cost.md#energy-mode)).
- **Color** comes from `Theme` roles. Color literals belong only in `theme.rs`
  and `scheme.rs`.
  - Every resting border and hairline is `theme.border`, on chrome frames and
    interactive content alike. The color scheme owns it, so never hardcode a
    frame color.
  - The exception is focus: the edge of a focused field, panel or card is
    `theme.border_focus`.
  - The selected tab is raised by its panel fill alone and has no border.
  - Accent means progress or focus. It is never a panel edge.
- **Selection** in the sidebar (workspace, worktree, agent) is the
  `theme.panel` pill with `panel_fg`/`muted` text and a bold name.
  `selection_bg`/`selection_fg` are for text selection and list highlighting,
  not sidebar rows.

## Motion

State changes ease, with the two exceptions listed after these rules:

- A floating panel (menu, picker, palette, popover, tooltip) or a page opens
  through `style::enter`. Wrap only its open branch, so the fade replays each
  time it appears.
- Hover and selection backgrounds go through `FillExt::hover_fill` /
  `state_fill`, never `.hover(|s| s.bg(..))` or a `.bg(..)` that depends on
  selection. Call them before adding children so the fill sits beneath.
- Use `style::chevron` for disclosure and `style::toggle` for switches.
- Status is a still dot in its color: `style::status_dot`, or
  `style::busy_dot` in place of a glyph while something works or loads.
  Nothing turns or pulses for as long as a state lasts, because every frame
  of such a loop redraws the whole window (see
  [idle-cost.md](idle-cost.md#transitions-and-repaints)).
- A new animation must count as moving in the tick loop and must honor
  `cx.reduce_motion()` (see [idle-cost.md](idle-cost.md)).

Exceptions:

- The terminal grid never takes an opacity fade, because a translucent window
  would show the desktop through it. `reflow_alpha` fades its text instead.
- Do not resize the daemon while the sidebar tweens. It reflows once when the
  dock moves; in-between sizes would make it reflow again.

## Windows and backdrop

- Every window root goes through `apply_chrome_font`, so the Interface family
  and size reach the whole tree. A new window opts in by taking the shared
  `Settings` and calling it in `render`.
- The macOS backdrop is `gui/glass.rs`'s own `NSVisualEffectView`, not GPUI's
  `Blurred` appearance, which is a weak, desaturated material by design. The
  window stays `Transparent`, and the blur slider drives the view's
  `alphaValue`. The view is pinned to the dark appearance, so a light-mode Mac
  still frosts dark under the chrome.

## Render before shipping

The `visual-test` feature, which is not part of a normal build, captures every
surface to `/tmp/pilus-shots`:

```sh
cargo run -p pilus --features visual-test -- --ui-shots       # standalone windows
cargo run -p pilus --features visual-test -- --ui-shots-main  # main window + overlays
```

`PILUS_SHOT_FONT_SIZE` overrides the Interface size for those captures. It is
the quickest way to catch chrome that ignores the setting.

Captures never touch the network or real data. Each feature has its own sample
hook: `sipil::preview`, `search_show_sample`, `database_show_sample`, and the
GitHub feed samples.
