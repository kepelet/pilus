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
  `scheme.rs` only. Chrome and floating-panel frames use
  `terminal_inset_border()`, the terminal card's low-contrast hairline;
  interactive content (fields, rows, buttons, badges) outlines with
  `theme.border`. The selected tab is raised by its panel fill alone; it takes
  no border. Accent means progress or focus, not a panel edge.
- **Selection** in the sidebar (workspace, worktree, agent) is the `theme.panel`
  pill with `panel_fg`/`muted` text and a bold name. `selection_bg`/`selection_fg`
  belong to text selection and list highlighting, not sidebar rows.
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
