# Working in this repository

`pilus` is a native desktop client for a Luvus daemon. The daemon renders its
entire TUI surface to clients as a grid of styled cells; `pilus` attaches, folds
that grid into a `Surface`, and paints it in a GPUI window.

## The one rule that bites

`crates/pilus-protocol` mirrors a binary wire format encoded with
`bincode`'s positional serde encoding. In that encoding, an enum's variant
index and a struct's field order **are** the wire format. Adding a field in the
middle of a struct, reordering enum variants, or deleting a variant will
silently corrupt every message after the change.

If you must change a message:

1. append new variants and fields at the end;
2. bump `PROTOCOL_VERSION`;
3. extend `crates/pilus-client/tests/handshake.rs` so the change is exercised
   against a scripted peer.

The enabling feature in `Cargo.toml` (`runtime_shaders`) is also deliberate: it
keeps the macOS build from shelling out to the `metal` compiler, so a machine
with only the command-line tools can build the app.

## Invariants

- `FrameData::validate` and `FrameData::apply_diff` are the only places that
  mutate a grid. A malformed diff returns an error and leaves the frame
  untouched, so a client can request a full frame instead of painting a torn
  surface.
- `pilus-client` owns no rendering. It emits `ClientEvent`s and folds
  `ServerMessage`s into `Surface`; the window decides how to draw.
- The GUI thread never blocks on socket IO. Reads happen on a background reader
  thread; writes go through an unbounded channel to a writer thread.
- The macOS backdrop is `gui/glass.rs`'s own `NSVisualEffectView`, not GPUI's
  `Blurred` appearance (a weak, desaturated material by design). The window
  stays `Transparent`; the blur slider drives the view's `alphaValue`. The view is pinned to the dark
  appearance, so a light-mode Mac still frosts dark under the chrome.

- The GitHub Repositories section reads the signed-in account through `gh` on
  the machine `pilus` runs on, never through a daemon or SSH. It shows five
  repositories: `Settings::github_pinned` first, then the most recently pushed
  to. Every list, and each repository's issues, pull requests and workflow
  runs, goes through `pilus_client::GhAccount`'s cache (fresh lists are free,
  REST lists revalidate by `ETag`, a rate limit is waited out and persisted in
  `github-cache.json`); do not call `gh` for these from the GUI directly, or the
  cache and its limits are bypassed. `Settings::github` and
  `github_repositories` switch the feature off, which means no requests at
  all. The `visual-test` captures fill the feed with samples so a shot never
  reaches GitHub.

## UI

`crates/pilus/src/gui/ui_playground.rs` is the visual contract: a working
catalogue of every native element the app draws. `crates/pilus/src/gui/style.rs`
is the only implementation of it. Draw controls through `style::button`,
`style::small_button`, `style::field`, and `style::section_label`; do not
hand-roll a button, field, or section label in another window. When an element
is missing, add it to `style.rs` and to the playground first, then use it.

- **Type scale**: 13 body, 12 secondary, 11 label. Section labels are bold,
  muted, and uppercase. Resolve sizes with `style::Fonts::resolve` or
  `style::scaled` so every surface follows the Interface size setting; a fixed
  `text_size(px(...))` for chrome text is a bug even when the default happens
  to match.
- **Geometry**: 28px regular controls, 24px compact ones, `px_3`/`px_2`
  padding, `rounded_md` for controls, cards, and overlays, `rounded_lg` for
  large floating panels. The Home composer's `rounded_t_lg`/`rounded_b_lg`
  follows that rule rather than excepting it.
- **Color** comes from `Theme` roles; literals belong in `theme.rs` and
  `scheme.rs` only. Every border and hairline, chrome frames and interactive
  content alike, is `theme.border`: the color scheme owns it, so never
  hardcode a frame color. The selected tab is raised by its panel fill alone; it takes
  no border. Accent means progress or focus, not a panel edge; the edge of a focused
  field, panel, or card is `theme.border_focus`.
- **Selection** in the sidebar (workspace, worktree, agent) is the `theme.panel`
  pill with `panel_fg`/`muted` text and a bold name. `selection_bg`/`selection_fg`
  belong to text selection and list highlighting, not sidebar rows.
- **Motion**: every state change eases; nothing snaps.
  - A floating panel (menu, picker, palette, popover, tooltip) or a page
    opens through `style::enter`, wrapped on its open branch only so the fade
    replays each time it appears.
  - Hover and selection backgrounds go through `FillExt::hover_fill` /
    `state_fill`, never `.hover(|s| s.bg(..))` or a selection-dependent
    `.bg(..)`. Call them before adding children so the fill sits beneath.
  - Disclosure uses `style::chevron`, switches `style::toggle`, and working
    status `style::spinner`.
  - The terminal grid never takes an opacity fade (a translucent window would
    show the desktop through it); `reflow_alpha` fades its text instead. Do
    not resize the daemon while the sidebar tweens: it reflows once when the
    dock moves, and in-between sizes make it reflow again.
- Every window root goes through `apply_chrome_font`, so the Interface family
  and size reach the whole tree. A new window opts in by taking the shared
  `Settings` and calling it in `render`.

## Commands

```sh
cargo test                    # unit + handshake integration tests
cargo run -p pilus              # the window (needs a running daemon)
cargo run -p pilus -- --probe   # headless: print the first frame
```

Before shipping a UI change, render it. The `visual-test` feature (not part of
a normal build) captures every surface to `/tmp/pilus-shots`:

```sh
cargo run -p pilus --features visual-test -- --ui-shots       # standalone windows
cargo run -p pilus --features visual-test -- --ui-shots-main  # main window + overlays
```

`PILUS_SHOT_FONT_SIZE` overrides the Interface size for those captures, which is
the quickest way to catch chrome that ignores the setting.

## Style

- Rust 2024, `unsafe_code` and `unwrap_used` are denied at the workspace level.
  The single `unsafe` call (`discovery::current_uid`) carries a `SAFETY` comment
  and a scoped `allow`.
- Comments state *why*, not *what*. Prefer a name that removes the need for the
  comment.
- Every module has a `//!` header explaining its role in the data flow.
