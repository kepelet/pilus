//! Syntax highlighting for the Diff and Git tabs' code lines.
//!
//! A patch arrives as isolated lines, so each hunk is parsed as two streams
//! the way a reader sees it: the old side (context and deletions) and the new
//! side (context and additions). Parsing the interleaved patch order would
//! feed a deleted line's state into the line that replaced it. The result is
//! a list of [`Span`]s per line, kept on the row so painting stays a lookup.
//!
//! Spans name a [`Token`] role, never a color: `diff.rs` resolves roles
//! through the active `Theme`, so a theme switch recolors without a reparse.

use std::ops::Range;
use std::path::Path;
use std::sync::OnceLock;

use pilus_client::{DiffHunk, DiffLineKind};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

/// Most lines of one file that are highlighted; a bigger patch paints plain
/// rather than stall the window on a parse.
const MAX_LINES: usize = 4000;
/// Longest line that is highlighted. Minified output is one enormous line and
/// is no easier to read in color.
const MAX_LINE_BYTES: usize = 600;

/// What a run of code is, independent of any palette.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Token {
    Keyword,
    String,
    Number,
    Comment,
    Function,
    Type,
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
            })
        })
        .as_ref()
}

/// The role of the innermost scope that names one, or `None` for plain code.
fn classify(stack: &ScopeStack) -> Option<Token> {
    let known = scopes()?;
    for scope in stack.as_slice().iter().rev() {
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

/// The grammar for `path`, by extension and then by whole file name
/// (`Makefile`, `Dockerfile`). `None` for a file with no known grammar.
fn syntax_for(path: &str) -> Option<&'static SyntaxReference> {
    let set = syntaxes();
    let path = Path::new(path);
    path.extension()
        .and_then(|extension| extension.to_str())
        .and_then(|extension| set.find_syntax_by_extension(extension))
        .or_else(|| {
            path.file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| set.find_syntax_by_extension(name))
        })
}

/// Highlight every line of one stream, returning spans per line.
fn stream<'a>(syntax: &SyntaxReference, lines: impl Iterator<Item = &'a str>) -> Vec<Vec<Span>> {
    let set = syntaxes();
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut out = Vec::new();
    for text in lines {
        if text.len() > MAX_LINE_BYTES {
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
            let Some(token) = classify(stack) else {
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
                let spans = stream(syntax, picked.iter().map(|&index| texts[index].as_str()));
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
}
