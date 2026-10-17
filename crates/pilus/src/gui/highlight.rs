//! Syntax highlighting for the Diff and Git tabs' code lines, and for the
//! Files tab editor's whole text.
//!
//! A patch arrives as isolated lines, so each hunk is parsed as two streams
//! the way a reader sees it: the old side (context and deletions) and the new
//! side (context and additions). Parsing the interleaved patch order would
//! feed a deleted line's state into the line that replaced it. The result is
//! a list of [`Span`]s per line, kept on the row so painting stays a lookup.
//!
//! A whole file is one stream, and its spans are offsets into the text rather
//! than into a line, which is what an editor paints from ([`source`]).
//!
//! Spans name a [`Token`] role, never a color: [`look`] resolves roles
//! through the active `Theme`, so a theme switch recolors without a reparse.

use std::ops::Range;
use std::path::Path;
use std::sync::OnceLock;

use gpui::Rgba;
use pilus_client::{DiffHunk, DiffLineKind};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

use super::md_view;
use super::theme::Theme;

/// Most lines of one file that are highlighted; a bigger patch paints plain
/// rather than stall the window on a parse.
const MAX_LINES: usize = 4000;
/// Longest line that is highlighted. Minified output is one enormous line and
/// is no easier to read in color.
const MAX_LINE_BYTES: usize = 600;
/// What the editor colors: a longer text paints plain rather than reparse on
/// every keystroke. Prose runs to long lines, so its line limit is higher.
const MAX_SOURCE_BYTES: usize = 300 * 1024;
const MAX_SOURCE_LINES: usize = 6000;
const MAX_SOURCE_LINE_BYTES: usize = 4000;

/// What a run of code is, independent of any palette.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Token {
    Keyword,
    String,
    Number,
    Comment,
    Function,
    Type,
    /// Markdown's own marks, which only [`source`] reports.
    Heading,
    Bold,
    Italic,
    Link,
    Raw,
}

/// How a role is drawn: a palette color, and whether it leans or thickens.
#[derive(Clone, Copy, Debug)]
pub(super) struct Look {
    pub(super) color: Rgba,
    pub(super) italic: bool,
    pub(super) bold: bool,
}

/// The look a syntax role takes in `theme`. Roles reuse the theme's ANSI
/// colors, which each scheme already tunes for code on its own background.
pub(super) fn look(token: Token, theme: Theme) -> Look {
    let (color, italic, bold) = match token {
        Token::Keyword => (theme.ansi[5], false, false),
        Token::String => (theme.ansi[2], false, false),
        Token::Number => (theme.ansi[3], false, false),
        Token::Function => (theme.ansi[4], false, false),
        Token::Type => (theme.ansi[6], false, false),
        Token::Comment => (theme.muted, true, false),
        Token::Heading => (theme.ansi[4], false, true),
        Token::Bold => (theme.fg, false, true),
        Token::Italic => (theme.fg, true, false),
        Token::Link => (theme.ansi[6], false, false),
        Token::Raw => (theme.ansi[3], false, false),
    };
    Look {
        color,
        italic,
        bold,
    }
}

/// A colored run within one line's text, in byte offsets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Span {
    pub(super) range: Range<usize>,
    pub(super) token: Token,
}

fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// Parse the scope selectors once: building a `Scope` per token
/// would dominate the cost of a long file.
struct Scopes {
    comment: Scope,
    string: Scope,
    string_punctuation: Scope,
    numeric: Scope,
    constant: Scope,
    keyword_operator: Scope,
    keyword: Scope,
    storage: Scope,
    function: Scope,
    support_function: Scope,
    call: Scope,
    type_names: [Scope; 5],
    support_type: Scope,
    support_class: Scope,
    /// The grammar's root, which an embedded language sits under.
    source: Scope,
    heading: Scope,
    bold: Scope,
    italic: Scope,
    raw: Scope,
    links: [Scope; 3],
    list_item: Scope,
    tag: Scope,
    attribute: Scope,
}

/// `None` if a selector fails to parse, which would be a typo in the literals
/// below: highlighting then stays off instead of coloring everything.
fn scopes() -> Option<&'static Scopes> {
    static SCOPES: OnceLock<Option<Scopes>> = OnceLock::new();
    SCOPES
        .get_or_init(|| {
            let scope = |name: &str| Scope::new(name).ok();
            Some(Scopes {
                comment: scope("comment")?,
                string: scope("string")?,
                string_punctuation: scope("punctuation.definition.string")?,
                numeric: scope("constant.numeric")?,
                constant: scope("constant")?,
                keyword_operator: scope("keyword.operator")?,
                keyword: scope("keyword")?,
                storage: scope("storage")?,
                function: scope("entity.name.function")?,
                support_function: scope("support.function")?,
                call: scope("meta.function-call")?,
                type_names: [
                    scope("entity.name.type")?,
                    scope("entity.name.class")?,
                    scope("entity.name.struct")?,
                    scope("entity.name.enum")?,
                    scope("entity.name.trait")?,
                ],
                support_type: scope("support.type")?,
                support_class: scope("support.class")?,
                source: scope("source")?,
                heading: scope("markup.heading")?,
                bold: scope("markup.bold")?,
                italic: scope("markup.italic")?,
                raw: scope("markup.raw")?,
                links: [
                    scope("markup.underline.link")?,
                    scope("string.other.link")?,
                    scope("meta.link")?,
                ],
                list_item: scope("punctuation.definition.list_item")?,
                tag: scope("entity.name.tag")?,
                attribute: scope("entity.other.attribute-name")?,
            })
        })
        .as_ref()
}

/// The role of the innermost scope that names one, or `None` for plain code.
///
/// `prose` also names markdown's marks. Those are checked first within a
/// scope, since a link's text is a string to the code roles below.
fn classify(stack: &ScopeStack, prose: bool) -> Option<Token> {
    let known = scopes()?;
    for scope in stack.as_slice().iter().rev() {
        if prose {
            if known.heading.is_prefix_of(*scope) {
                return Some(Token::Heading);
            }
            if known.bold.is_prefix_of(*scope) {
                return Some(Token::Bold);
            }
            if known.italic.is_prefix_of(*scope) {
                return Some(Token::Italic);
            }
            if known.links.iter().any(|link| link.is_prefix_of(*scope)) {
                return Some(Token::Link);
            }
            if known.list_item.is_prefix_of(*scope) {
                return Some(Token::Keyword);
            }
            if known.raw.is_prefix_of(*scope) {
                return Some(Token::Raw);
            }
            // Embedded HTML, and the JSX an MDX file writes the same way.
            if known.tag.is_prefix_of(*scope) {
                return Some(Token::Type);
            }
            if known.attribute.is_prefix_of(*scope) {
                return Some(Token::Function);
            }
        }
        // Reaching the root of an embedded language means none of its own
        // scopes named a role, so the text is plain code, not the fence's.
        if known.source.is_prefix_of(*scope) {
            return None;
        }
        if known.comment.is_prefix_of(*scope) {
            return Some(Token::Comment);
        }
        if known.string.is_prefix_of(*scope) || known.string_punctuation.is_prefix_of(*scope) {
            return Some(Token::String);
        }
        if known.numeric.is_prefix_of(*scope) {
            return Some(Token::Number);
        }
        // Operators read as plain code; coloring every `=` and `.` is noise.
        if known.keyword_operator.is_prefix_of(*scope) {
            return None;
        }
        if known.keyword.is_prefix_of(*scope) || known.storage.is_prefix_of(*scope) {
            return Some(Token::Keyword);
        }
        if known.function.is_prefix_of(*scope)
            || known.support_function.is_prefix_of(*scope)
            || known.call.is_prefix_of(*scope)
        {
            return Some(Token::Function);
        }
        if known
            .type_names
            .iter()
            .any(|name| name.is_prefix_of(*scope))
            || known.support_type.is_prefix_of(*scope)
            || known.support_class.is_prefix_of(*scope)
        {
            return Some(Token::Type);
        }
        if known.constant.is_prefix_of(*scope) {
            return Some(Token::Number);
        }
    }
    None
}

/// Names the bundled grammars lack, read as the nearest one they have. To a
/// highlighter that colors keywords, strings and comments, TypeScript and JSX
/// are JavaScript.
const ALIASES: [(&str, &str); 10] = [
    ("ts", "js"),
    ("tsx", "js"),
    ("mts", "js"),
    ("cts", "js"),
    ("jsx", "js"),
    ("mjs", "js"),
    ("cjs", "js"),
    ("typescript", "js"),
    ("jsonc", "json"),
    ("json5", "json"),
];

/// `name` as the bundled grammars know it.
fn alias(name: &str) -> &str {
    ALIASES
        .iter()
        .find(|(from, _)| from.eq_ignore_ascii_case(name))
        .map_or(name, |(_, to)| to)
}

/// Extensions and file names that are text without a grammar to color them, so
/// they still open in the editor.
const PLAIN_TEXT: [&str; 17] = [
    "toml",
    "lock",
    "env",
    "ini",
    "cfg",
    "conf",
    "gitignore",
    "gitattributes",
    "editorconfig",
    "kt",
    "swift",
    "vue",
    "svelte",
    "astro",
    "zig",
    "Dockerfile",
    "LICENSE",
];

/// Whether `path` is a file the Files tab opens in its editor: markdown, a file
/// with a grammar, or a known kind of plain text. Anything else, which may be
/// an image or a binary, is left to the daemon to show.
pub(super) fn is_text(path: &str) -> bool {
    if md_view::is_markdown(path) || syntax_for(path).is_some() {
        return true;
    }
    let path = Path::new(path);
    // A dotfile such as `.env` has a name and no extension.
    let named = |part: Option<&std::ffi::OsStr>| {
        part.and_then(|part| part.to_str()).is_some_and(|part| {
            let part = part.trim_start_matches('.');
            PLAIN_TEXT
                .iter()
                .any(|known| known.eq_ignore_ascii_case(part))
        })
    };
    named(path.extension()) || named(path.file_name())
}

/// The grammar for `path`, by extension and then by whole file name
/// (`Makefile`, `Dockerfile`). `None` for a file with no known grammar.
fn syntax_for(path: &str) -> Option<&'static SyntaxReference> {
    let set = syntaxes();
    // The bundled grammar knows only some spellings of markdown; every file
    // the Files tab opens as a document, `.mdx` included, reads as it.
    if md_view::is_markdown(path) {
        return set.find_syntax_by_extension("md");
    }
    let path = Path::new(path);
    path.extension()
        .and_then(|extension| extension.to_str())
        .and_then(|extension| set.find_syntax_by_extension(alias(extension)))
        .or_else(|| {
            path.file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| set.find_syntax_by_extension(name))
        })
}

/// Whether `syntax` is markdown, which has marks of its own.
fn is_prose(syntax: &SyntaxReference) -> bool {
    syntax.name == "Markdown"
}

/// Highlight every line of one stream, returning spans per line. A line over
/// `max_line` bytes is left plain.
fn stream<'a>(
    syntax: &SyntaxReference,
    lines: impl Iterator<Item = &'a str>,
    max_line: usize,
    prose: bool,
) -> Vec<Vec<Span>> {
    let set = syntaxes();
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut out = Vec::new();
    for text in lines {
        if text.len() > max_line {
            out.push(Vec::new());
            continue;
        }
        // The grammars are the `newlines` flavor, which expects each line to
        // end in one.
        let mut line = String::with_capacity(text.len() + 1);
        line.push_str(text);
        line.push('\n');
        let Ok(ops) = state.parse_line(&line, set) else {
            // A grammar that errors mid-file would leave later lines wrong;
            // paint this one plain and start the next from a clean stack.
            out.push(Vec::new());
            state = ParseState::new(syntax);
            stack = ScopeStack::new();
            continue;
        };
        let mut spans: Vec<Span> = Vec::new();
        let mut at = 0;
        let mut push = |from: usize, to: usize, stack: &ScopeStack| {
            let (from, to) = (from.min(text.len()), to.min(text.len()));
            if from >= to {
                return;
            }
            let Some(token) = classify(stack, prose) else {
                return;
            };
            match spans.last_mut() {
                Some(last) if last.token == token && last.range.end == from => {
                    last.range.end = to;
                }
                _ => spans.push(Span {
                    range: from..to,
                    token,
                }),
            }
        };
        for (offset, op) in ops {
            push(at, offset, &stack);
            at = offset;
            // A malformed op list only costs this line its color.
            if stack.apply(&op).is_err() {
                stack = ScopeStack::new();
            }
        }
        push(at, text.len(), &stack);
        out.push(spans);
    }
    out
}

/// Spans for every line of `hunks`' file at `path`, one list per hunk line in
/// patch order: `result[hunk][line]`. Empty lists when the file has no known
/// grammar or is too large to color.
pub(super) fn hunks(path: &str, hunks: &[DiffHunk]) -> Vec<Vec<Vec<Span>>> {
    let plain = || {
        hunks
            .iter()
            .map(|hunk| vec![Vec::new(); hunk.lines.len()])
            .collect()
    };
    let total: usize = hunks.iter().map(|hunk| hunk.lines.len()).sum();
    if total > MAX_LINES {
        return plain();
    }
    let Some(syntax) = syntax_for(path) else {
        return plain();
    };
    hunks
        .iter()
        .map(|hunk| {
            // Tabs are widened for display before painting, so offsets must
            // be measured on the same text.
            let texts: Vec<String> = hunk
                .lines
                .iter()
                .map(|line| line.text.replace('\t', "    "))
                .collect();
            let side = |keep: DiffLineKind| {
                let picked: Vec<usize> = hunk
                    .lines
                    .iter()
                    .enumerate()
                    .filter(|(_, line)| line.kind == DiffLineKind::Context || line.kind == keep)
                    .map(|(index, _)| index)
                    .collect();
                let spans = stream(
                    syntax,
                    picked.iter().map(|&index| texts[index].as_str()),
                    MAX_LINE_BYTES,
                    false,
                );
                (picked, spans)
            };
            let (old_lines, old_spans) = side(DiffLineKind::Deletion);
            let (new_lines, new_spans) = side(DiffLineKind::Addition);
            let mut lines = vec![Vec::new(); hunk.lines.len()];
            for (index, spans) in old_lines.into_iter().zip(old_spans) {
                lines[index] = spans;
            }
            // Context appears in both streams; the new side wins, and every
            // addition only exists there.
            for (index, spans) in new_lines.into_iter().zip(new_spans) {
                lines[index] = spans;
            }
            lines
        })
        .collect()
}

/// The fenced blocks of markdown `lines`, as the line ranges of their bodies
/// and the language their opening names. The bundled grammar colors a fence
/// as one block of code, so the body is parsed again as its own language.
fn fences<'a>(lines: &[&'a str]) -> Vec<(Range<usize>, &'a str)> {
    let mut found = Vec::new();
    let mut open: Option<(usize, &str, char, usize)> = None;
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        let Some(mark) = trimmed
            .chars()
            .next()
            .filter(|mark| matches!(mark, '`' | '~'))
        else {
            continue;
        };
        let run = trimmed.chars().take_while(|&c| c == mark).count();
        if run < 3 {
            continue;
        }
        let rest = trimmed[run..].trim();
        match open {
            None => {
                let language = rest.split_whitespace().next().unwrap_or("");
                open = Some((index + 1, language, mark, run));
            }
            Some((start, language, opened, width))
                if mark == opened && run >= width && rest.is_empty() =>
            {
                found.push((start..index, language));
                open = None;
            }
            Some(_) => {}
        }
    }
    // A fence left open runs to the end, as markdown reads it.
    if let Some((start, language, _, _)) = open {
        found.push((start..lines.len(), language));
    }
    found
}

/// Replace the coloring of `lines[body]` with that of the grammar `language`
/// names, if one is known. Each line is parsed from a fresh state: the body is
/// a language of its own, not a continuation of the markdown around it.
fn recolor(lines: &[&str], per_line: &mut [Vec<Span>], body: Range<usize>, language: &str) {
    let Some(syntax) = syntaxes().find_syntax_by_token(alias(language)) else {
        return;
    };
    let colored = stream(
        syntax,
        lines[body.clone()].iter().copied(),
        MAX_SOURCE_LINE_BYTES,
        false,
    );
    for (index, spans) in body.zip(colored) {
        per_line[index] = spans;
    }
}

/// The lines of a leading `---` front matter block, delimiters included.
/// A block left open is not front matter, since markdown reads it as a rule.
fn front_matter(lines: &[&str]) -> Option<Range<usize>> {
    if lines.first()?.trim_end() != "---" {
        return None;
    }
    let close = lines[1..]
        .iter()
        .position(|line| matches!(line.trim_end(), "---" | "..."))?;
    Some(0..close + 2)
}

/// The `import` and `export` blocks of an MDX file, as line ranges. A block
/// starts on a line of its own after a blank one and runs to the next blank
/// line, as MDX reads it. `skip` are the fences, whose text is never ESM.
fn esm_blocks(lines: &[&str], from: usize, skip: &[Range<usize>]) -> Vec<Range<usize>> {
    let mut found = Vec::new();
    let mut index = from;
    let mut after_blank = true;
    while index < lines.len() {
        let line = lines[index];
        let in_fence = skip.iter().any(|body| body.contains(&index));
        let starts = after_blank
            && !in_fence
            && (line.starts_with("import ") || line.starts_with("export "));
        if starts {
            let end = lines[index..]
                .iter()
                .position(|line| line.trim().is_empty())
                .map_or(lines.len(), |blank| index + blank);
            found.push(index..end);
            index = end;
            continue;
        }
        after_blank = line.trim().is_empty();
        index += 1;
    }
    found
}

/// Spans for the whole of `text`, as offsets into it, for the file at `path`.
/// Empty when the file has no known grammar or is too large to color on each
/// keystroke.
pub(super) fn source(path: &str, text: &str) -> Vec<Span> {
    if text.len() > MAX_SOURCE_BYTES {
        return Vec::new();
    }
    let Some(syntax) = syntax_for(path) else {
        return Vec::new();
    };
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.len() > MAX_SOURCE_LINES {
        return Vec::new();
    }
    let prose = is_prose(syntax);
    // Front matter is parsed apart from the markdown below it: its closing
    // `---` would otherwise underline the last key as a heading.
    let front = if prose { front_matter(&lines) } else { None };
    let body_from = front.as_ref().map_or(0, |front| front.end);
    let mut per_line = vec![Vec::new(); body_from];
    per_line.extend(stream(
        syntax,
        lines[body_from..].iter().copied(),
        MAX_SOURCE_LINE_BYTES,
        prose,
    ));
    if let Some(front) = front {
        recolor(
            &lines,
            &mut per_line,
            front.start + 1..front.end - 1,
            "yaml",
        );
    }
    if prose {
        let fenced = fences(&lines);
        let bodies: Vec<Range<usize>> = fenced.iter().map(|(body, _)| body.clone()).collect();
        for (body, language) in fenced {
            recolor(&lines, &mut per_line, body, language);
        }
        if md_view::is_mdx(path) {
            for block in esm_blocks(&lines, body_from, &bodies) {
                recolor(&lines, &mut per_line, block, "js");
            }
        }
    }
    let mut spans = Vec::new();
    let mut line_start = 0;
    for (line, found) in lines.iter().zip(per_line) {
        for span in found {
            spans.push(Span {
                range: line_start + span.range.start..line_start + span.range.end,
                token: span.token,
            });
        }
        line_start += line.len() + 1;
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilus_client::DiffLine;

    fn line(kind: DiffLineKind, text: &str) -> DiffLine {
        DiffLine {
            kind,
            old_line: None,
            new_line: None,
            text: text.to_string(),
        }
    }

    fn hunk(lines: Vec<DiffLine>) -> DiffHunk {
        DiffHunk {
            header: "@@".to_string(),
            lines,
        }
    }

    fn tokens(spans: &[Span]) -> Vec<Token> {
        spans.iter().map(|span| span.token).collect()
    }

    #[test]
    fn rust_lines_get_keyword_string_and_comment_spans() {
        let result = hunks(
            "src/lib.rs",
            &[hunk(vec![line(
                DiffLineKind::Addition,
                "let name = \"pilus\"; // note",
            )])],
        );
        let spans = &result[0][0];
        let found = tokens(spans);
        assert!(found.contains(&Token::Keyword), "{spans:?}");
        assert!(found.contains(&Token::String), "{spans:?}");
        assert!(found.contains(&Token::Comment), "{spans:?}");
        let string = spans
            .iter()
            .find(|span| span.token == Token::String)
            .expect("string");
        assert_eq!(
            &"let name = \"pilus\"; // note"[string.range.clone()],
            "\"pilus\""
        );
    }

    #[test]
    fn an_unknown_extension_stays_plain() {
        let result = hunks(
            "notes.zzz",
            &[hunk(vec![line(DiffLineKind::Addition, "let x = 1;")])],
        );
        assert!(result[0][0].is_empty());
    }

    #[test]
    fn deletions_and_additions_parse_on_their_own_side() {
        // The deleted line opens a block comment the addition never sees; if
        // the sides were interleaved, the addition would paint as comment.
        let result = hunks(
            "a.rs",
            &[hunk(vec![
                line(DiffLineKind::Deletion, "/* start"),
                line(DiffLineKind::Addition, "let x = 1;"),
            ])],
        );
        assert!(tokens(&result[0][1]).contains(&Token::Keyword));
        assert!(!tokens(&result[0][1]).contains(&Token::Comment));
    }

    #[test]
    fn one_list_per_line_even_when_nothing_is_colored() {
        let result = hunks(
            "a.rs",
            &[hunk(vec![
                line(DiffLineKind::Context, ""),
                line(DiffLineKind::Context, "}"),
            ])],
        );
        assert_eq!(result[0].len(), 2);
    }

    fn marked<'a>(text: &'a str, spans: &[Span], token: Token) -> Vec<&'a str> {
        spans
            .iter()
            .filter(|span| span.token == token)
            .map(|span| &text[span.range.clone()])
            .collect()
    }

    #[test]
    fn markdown_marks_are_colored_in_place() {
        let text = "# Title\n\nSome **bold** and *lean* text with `code` and [a link](https://x.y).\n\n- item\n";
        let spans = source("note.md", text);
        let heading = marked(text, &spans, Token::Heading);
        assert!(heading.iter().any(|run| run.contains("Title")), "{spans:?}");
        assert!(
            marked(text, &spans, Token::Bold)
                .iter()
                .any(|run| run.contains("bold"))
        );
        assert!(
            marked(text, &spans, Token::Italic)
                .iter()
                .any(|run| run.contains("lean"))
        );
        assert!(
            marked(text, &spans, Token::Raw)
                .iter()
                .any(|run| run.contains("code"))
        );
        assert!(
            marked(text, &spans, Token::Link)
                .iter()
                .any(|run| run.contains("a link"))
        );
        assert!(
            marked(text, &spans, Token::Keyword).contains(&"-"),
            "{spans:?}"
        );
    }

    #[test]
    fn a_fenced_block_takes_its_languages_colors() {
        let text = "```rust\nlet x = 1;\n```\n";
        let spans = source("note.md", text);
        let keywords = marked(text, &spans, Token::Keyword);
        assert!(keywords.contains(&"let"), "{spans:?}");
        // The identifier is plain code, not the fence's color.
        assert!(
            !marked(text, &spans, Token::Raw).contains(&"x"),
            "{spans:?}"
        );
    }

    #[test]
    fn source_offsets_run_across_lines() {
        let text = "// one\nlet a = 1;\n";
        let spans = source("a.rs", text);
        let comment = spans
            .iter()
            .find(|span| span.token == Token::Comment)
            .expect("comment");
        assert_eq!(&text[comment.range.clone()], "// one");
        let keyword = spans
            .iter()
            .find(|span| span.token == Token::Keyword)
            .expect("keyword");
        assert_eq!(&text[keyword.range.clone()], "let");
    }

    const MDX: &str = "---\ntitle: Hi\n---\n\nimport { Chart } from './chart'\nexport const meta = { a: 1 }\n\n# Title\n\n<Chart year=\"2018\" />\n\nSome *lean* text.\n\n```js\nimport x from 'y'\n```\n";

    #[test]
    fn markdown_spellings_share_one_grammar() {
        for name in ["a.md", "a.markdown", "a.mkd", "a.mdx", "A.MDX"] {
            let spans = source(name, "# Title\n");
            assert_eq!(spans.len(), 1, "{name}: {spans:?}");
            assert_eq!(spans[0].token, Token::Heading, "{name}");
        }
    }

    #[test]
    fn mdx_imports_and_exports_take_javascripts_colors() {
        let spans = source("a.mdx", MDX);
        let keywords = marked(MDX, &spans, Token::Keyword);
        assert!(keywords.contains(&"import"), "{spans:?}");
        assert!(keywords.contains(&"export"), "{spans:?}");
        assert!(keywords.contains(&"const"), "{spans:?}");
        assert!(marked(MDX, &spans, Token::String).contains(&"'./chart'"));
    }

    #[test]
    fn an_import_is_only_code_in_mdx() {
        let text = "import x from 'y'\n";
        assert!(marked(text, &source("a.mdx", text), Token::Keyword).contains(&"import"),);
        assert!(source("a.md", text).is_empty());
    }

    #[test]
    fn an_import_inside_a_fence_is_the_fences_own() {
        // The fence is JavaScript either way; what matters is that the ESM
        // pass leaves it alone, so it is colored once and from the right start.
        let spans = source("a.mdx", MDX);
        let fenced = MDX.find("import x").expect("fence body");
        let hits = spans
            .iter()
            .filter(|span| span.range.start == fenced && span.token == Token::Keyword)
            .count();
        assert_eq!(hits, 1, "{spans:?}");
    }

    #[test]
    fn jsx_tags_are_colored_as_markup() {
        let spans = source("a.mdx", MDX);
        assert!(
            marked(MDX, &spans, Token::Type).contains(&"Chart"),
            "{spans:?}"
        );
        assert!(
            marked(MDX, &spans, Token::Function).contains(&"year"),
            "{spans:?}"
        );
    }

    #[test]
    fn front_matter_is_yaml_and_does_not_make_a_heading() {
        let spans = source("a.mdx", MDX);
        assert!(
            marked(MDX, &spans, Token::Heading)
                .iter()
                .all(|run| !run.contains("---")),
            "{spans:?}"
        );
        assert!(
            spans.iter().any(|span| &MDX[span.range.clone()] == "title"),
            "the key is colored as YAML: {spans:?}"
        );
    }

    #[test]
    fn a_rule_is_not_front_matter() {
        assert_eq!(front_matter(&["---", "text"]), None);
        assert_eq!(front_matter(&["text", "---", "a", "---"]), None);
        assert_eq!(front_matter(&["---", "a: 1", "---", "x"]), Some(0..3));
    }

    #[test]
    fn typescript_and_jsx_borrow_javascripts_grammar() {
        for name in ["a.ts", "a.tsx", "a.jsx", "a.mjs"] {
            let text = "const x = 'y' // note\n";
            let spans = source(name, text);
            assert!(
                marked(text, &spans, Token::Keyword).contains(&"const"),
                "{name}"
            );
            assert!(
                marked(text, &spans, Token::Comment).contains(&"// note"),
                "{name}"
            );
        }
        let fenced = "```ts\nconst x = 1\n```\n";
        assert!(marked(fenced, &source("a.md", fenced), Token::Keyword).contains(&"const"));
    }

    #[test]
    fn the_editor_takes_text_and_leaves_the_rest_to_the_daemon() {
        for text in [
            "a.rs",
            "a.md",
            "a.mdx",
            "a.ts",
            "Cargo.toml",
            "Cargo.lock",
            "Makefile",
            "Dockerfile",
            "dir/.env",
            "notes.txt",
        ] {
            assert!(is_text(text), "{text}");
        }
        for other in ["logo.png", "app.zip", "clip.mp4", "a.wasm", "noext"] {
            assert!(!is_text(other), "{other}");
        }
    }

    #[test]
    fn plain_text_and_unknown_files_stay_plain() {
        assert!(source("notes.txt", "# not a heading\n").is_empty());
        assert!(source("data.zzz", "let x = 1;").is_empty());
    }

    #[test]
    fn a_text_too_large_to_color_paints_plain() {
        let text = "let x = 1;\n".repeat(MAX_SOURCE_BYTES / 10);
        assert!(source("a.rs", &text).is_empty());
    }
}
