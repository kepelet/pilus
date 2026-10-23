# Files tab and editor

Read this before you change the Files tab (`files_tab.rs`), the editor, quick
open, find in file, or markdown notes.

## Opening and saving

The Files tab edits text files in place. Saves go through
`pilus_client::write_text` (local or over SSH), never through the daemon.

- A click opens a file in the editor when `highlight::is_text` says it is text:
  markdown, a file with a grammar, or a known plain-text kind. Any other file,
  and any double click, goes to the daemon.
- Markdown (`md`, `markdown`, `mdown`, `mkd`, `mdx`, by `md_view::is_markdown`)
  opens as a document with Source, Preview and side-by-side modes.
- Any other text file opens straight into the source editor, with no preview,
  and its `MdDoc` holds the text unparsed.
- A truncated read, or a file larger than `md_view::EDIT_LIMIT`, is not opened
  for editing.

## Highlighting

The editor field (`md_editor_input`) is colored by `highlight::source`.

- Markdown gets its marks. Front matter is colored as YAML and fenced blocks in
  their own language. In `.mdx`, `import`/`export` blocks are colored as
  JavaScript and JSX tags as markup.
- The bundled grammars lack TypeScript and JSX, so `highlight::ALIASES` reads
  them as JavaScript. TOML and a few others open uncolored.
- The preview reads `.mdx` with MDX's constructs (`markdown::mdx_options`) and
  falls back to plain markdown when MDX rejects the text, since a stray `<` or
  `{` is an MDX error. Do not make that parse stricter, or a half-typed file
  blanks the preview.

## Mermaid diagrams

A `mermaid` fence in a markdown preview, a GitHub body or a sipil chat message
is drawn as a diagram by `gui/mermaid.rs`.

- The first sight of a diagram starts a worker thread (`mermaid-rs-renderer`
  lays it out, resvg draws it) and the fence shows as a code card until the
  bitmap lands. `PilusView::poll` repaints when `mermaid::generation` moves.
  A source that does not parse stays a code card, so a half-typed diagram never
  blanks the preview.
- Bitmaps are cached by source and palette, 24 at most, least recently used
  out. A theme or Interface font change draws the diagram again.
- Colors come from `Theme` roles through `mermaid::Style`. Do not add a color
  literal there.
- The source is capped at `MAX_SOURCE`, and the SVG is read with
  `about::untrusted_svg_options` (see [security.md](../security.md)).
- `mermaid-rs-renderer` copies the system font it measures with into
  `~/.cache/mmdr/font-cache`, outside `~/.pilus`. We cannot turn that off.

## Quick open (⌘P) and find (⌘F)

Both belong to the Files tab.

- Quick open (`gui/quick_open.rs`) reads its own listing of the tree's root
  with `pilus_client::list_files`, because the daemon's tree only carries open
  folders. The listing uses `git ls-files` in a repository and `find`
  elsewhere, is capped at `MAX_FILES`, and runs locally or over SSH on a worker
  thread. It is read again each time the picker opens. A pick goes through
  `files_open_path`, like a tree click.
- Find in file (`gui/file_find.rs`) matches the editor's text. It is
  smart-case, and folds ASCII only so byte offsets stay valid.
  - It repaints only when `TextInput::revision` or the query changed.
  - It paints its matches through `TextInput::set_marks` and shows the current
    match as the editor's selection.
  - On a markdown preview, ⌘F switches to the source first.

## Editor key bindings

Editor chords are scoped by key context, never by a check in the handler. The
editor div carries the `MdEditor` context, so Cmd+S saves instead of toggling
the sidebar. A deeper context wins over a global binding, so a new editor chord
is just another `KeyBinding` in that context.

## Notes on markdown excerpts (`gui/md_notes.rs`)

- These notes belong to the app. They are kept per machine and per file in the
  `md_notes` table of `pilus.db`.
- They are not the daemon's review notes. Review notes are anchored to a Git
  layer, which an unchanged file does not have.
- Sending ticked notes is a single `agent.prompt` that carries every excerpt,
  so an agent never receives a note it cannot place.
