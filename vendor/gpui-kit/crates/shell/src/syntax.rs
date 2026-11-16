// HERDR: built-in pure-Rust syntax highlighting for script editors, adapted from Base's showcase.
use std::{collections::HashMap, ops::Range, sync::LazyLock};

use gpui::{Context, HighlightStyle, SharedString, Window};
use gpui_base::input::{
    EditorState, FoldRange, HighlightStyleResolver, InputEdit, InputHighlighter, Rope,
};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

/// Loading the default syntax definitions deserializes a few megabytes, so share
/// one set across every highlighter instance.
static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);

/// A small, WASM-compatible example adapter for Base's parser-independent
/// highlighting API. Syntect identifies scopes; the application-owned resolver
/// supplies all colors and font styles.
pub(crate) struct SyntectHighlighter {
    language: SharedString,
    /// Per-line parser state and highlights, so an edit reparses only from the
    /// edited line until the parser state matches the cached state again.
    lines: Option<LineCache>,
    fold_ranges: Vec<FoldRange>,
}

impl SyntectHighlighter {
    pub(crate) fn new(language: &str) -> Option<Self> {
        find_syntax(language)?;

        Some(Self {
            language: language.to_owned().into(),
            lines: None,
            fold_ranges: Vec::new(),
        })
    }
}

/// Scope ids are cheap to compare but expensive to stringify, so remember the
/// semantic name each one maps to.
#[derive(Default)]
struct SemanticNames(HashMap<Scope, Option<&'static str>>);

impl SemanticNames {
    fn for_scopes(&mut self, scopes: &ScopeStack) -> Option<&'static str> {
        scopes.scopes.iter().rev().find_map(|scope| {
            *self
                .0
                .entry(*scope)
                .or_insert_with(|| semantic_name(*scope))
        })
    }
}

/// Parses one line, appending highlights offset by `base`.
fn parse_line(
    line: &str,
    base: usize,
    parser: &mut ParseState,
    scopes: &mut ScopeStack,
    names: &mut SemanticNames,
    out: &mut Vec<(Range<usize>, &'static str)>,
) {
    let mut push = |range: Range<usize>, scopes: &ScopeStack, out: &mut Vec<_>| {
        if range.is_empty() {
            return;
        }
        if let Some(name) = names.for_scopes(scopes) {
            out.push((range, name));
        }
    };
    if let Ok(operations) = parser.parse_line(line, &SYNTAX_SET) {
        let mut cursor = 0;
        for (index, operation) in operations {
            push(base + cursor..base + index, scopes, out);
            let _ = scopes.apply(&operation);
            cursor = index;
        }
        push(base + cursor..base + line.len(), scopes, out);
    }
}

struct CachedLine {
    /// Parser and scope state at the start of the line.
    state: (ParseState, ScopeStack),
    /// Highlights relative to the line start, ordered and non-overlapping.
    highlights: Vec<(Range<usize>, &'static str)>,
}

/// Line `i` spans `starts[i]..starts[i + 1]` (the last one runs to the end of
/// the text), so a text with `n` newlines has `n + 1` lines.
struct LineCache {
    syntax: &'static SyntaxReference,
    names: SemanticNames,
    starts: Vec<usize>,
    lines: Vec<CachedLine>,
    /// Lines parsed by the last update, for tests.
    parsed_rows: usize,
}

impl LineCache {
    fn new(syntax: &'static SyntaxReference, text: &str) -> Self {
        let mut cache = Self {
            syntax,
            names: SemanticNames::default(),
            starts: Vec::new(),
            lines: Vec::new(),
            parsed_rows: 0,
        };
        cache.reparse_all(text);
        cache
    }

    fn line<'a>(&self, text: &'a str, row: usize) -> &'a str {
        let end = self.starts.get(row + 1).copied().unwrap_or(text.len());
        &text[self.starts[row]..end]
    }

    fn reparse_all(&mut self, text: &str) {
        self.starts = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(index, _)| index + 1))
            .collect();
        self.lines.clear();
        self.parsed_rows = 0;
        let mut parser = ParseState::new(self.syntax);
        let mut scopes = ScopeStack::new();
        for row in 0..self.starts.len() {
            self.parse_row(text, row, &mut parser, &mut scopes);
        }
    }

    /// Parses `row` (which must be the next line to push) and caches it.
    fn parse_row(
        &mut self,
        text: &str,
        row: usize,
        parser: &mut ParseState,
        scopes: &mut ScopeStack,
    ) {
        let state = (parser.clone(), scopes.clone());
        let mut highlights = Vec::new();
        self.parsed_rows += 1;
        let line = self.line(text, row);
        if !line.is_empty() {
            parse_line(line, 0, parser, scopes, &mut self.names, &mut highlights);
        }
        self.lines.push(CachedLine { state, highlights });
    }

    /// Applies one edit; `text` is the document after it. Lines before the edit
    /// keep their state; the edited lines are reparsed, and later lines are
    /// reparsed only until the parser state converges with their cached state.
    fn apply(&mut self, edit: &InputEdit, text: &str) {
        let start_row = edit.start_position.row;
        let old_end_row = edit.old_end_position.row;
        let new_end_row = edit.new_end_position.row;
        let consistent = start_row <= old_end_row
            && old_end_row < self.lines.len()
            && edit.start_byte <= edit.new_end_byte
            && edit.new_end_byte <= text.len()
            && self.starts[start_row] <= edit.start_byte;
        if !consistent {
            return self.reparse_all(text);
        }

        let delta = edit.new_end_byte as isize - edit.old_end_byte as isize;
        let first = self.starts[start_row];
        let mut starts = self.starts[..=start_row].to_vec();
        starts.extend(
            text[first..edit.new_end_byte]
                .match_indices('\n')
                .map(|(index, _)| first + index + 1),
        );
        if starts.len() != start_row + 1 + (new_end_row - start_row) {
            return self.reparse_all(text);
        }
        starts.extend(
            self.starts[old_end_row + 1..]
                .iter()
                .map(|start| start.wrapping_add_signed(delta)),
        );

        let (mut parser, mut scopes) = self.lines[start_row].state.clone();
        self.parsed_rows = 0;
        let mut tail = self
            .lines
            .drain(old_end_row + 1..)
            .collect::<Vec<_>>()
            .into_iter();
        self.lines.truncate(start_row);
        self.starts = starts;

        let mut row = start_row;
        while row <= new_end_row {
            self.parse_row(text, row, &mut parser, &mut scopes);
            row += 1;
        }
        while row < self.starts.len() {
            let Some(cached) = tail.next() else { break };
            if cached.state.0 == parser && cached.state.1 == scopes {
                self.lines.push(cached);
                self.lines.extend(tail);
                return;
            }
            self.parse_row(text, row, &mut parser, &mut scopes);
            row += 1;
        }
        if self.lines.len() != self.starts.len() {
            self.reparse_all(text);
        }
    }

    /// Absolute highlights overlapping `range`, ordered and non-overlapping.
    fn highlights_in(&self, range: &Range<usize>) -> Vec<(Range<usize>, &'static str)> {
        let first = self
            .starts
            .partition_point(|start| *start <= range.start)
            .saturating_sub(1);
        let mut out = Vec::new();
        for row in first..self.lines.len() {
            let base = self.starts[row];
            if base >= range.end {
                break;
            }
            for (highlight, name) in &self.lines[row].highlights {
                let absolute = base + highlight.start..base + highlight.end;
                if absolute.end > range.start && absolute.start < range.end {
                    out.push((absolute, *name));
                }
            }
        }
        out
    }
}

fn find_syntax(language: &str) -> Option<&'static SyntaxReference> {
    SYNTAX_SET
        .find_syntax_by_token(language)
        .or_else(|| SYNTAX_SET.find_syntax_by_extension(language))
}

impl InputHighlighter for SyntectHighlighter {
    fn language(&self) -> SharedString {
        self.language.clone()
    }

    fn update(
        &mut self,
        edit: Option<InputEdit>,
        text: &Rope,
        folding: bool,
        _window: &mut Window,
        _cx: &mut Context<EditorState>,
    ) {
        // Read the rope once and reuse that string for folding too.
        let text = text.to_string();
        let syntax = find_syntax(self.language.as_ref())
            .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());
        match (&mut self.lines, edit) {
            (Some(lines), Some(edit)) => lines.apply(&edit, &text),
            _ => self.lines = Some(LineCache::new(syntax, &text)),
        }

        self.fold_ranges = if folding {
            brace_fold_ranges(&text)
        } else {
            Vec::new()
        };
    }

    fn styles(
        &self,
        range: &Range<usize>,
        resolver: &dyn HighlightStyleResolver,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        let highlights = self
            .lines
            .as_ref()
            .map(|lines| lines.highlights_in(range))
            .unwrap_or_default();
        resolve_styles(&highlights, range, resolver)
    }

    fn fold_ranges(&self, _: &Rope) -> Vec<FoldRange> {
        self.fold_ranges.clone()
    }

    fn fold_ranges_for_edit(&self, _: Range<usize>, _: &Rope) -> Vec<FoldRange> {
        self.fold_ranges.clone()
    }
}

/// Turn the highlights overlapping `range` into gap-free style runs.
///
/// `highlights` is ordered and non-overlapping, so the first candidate is found
/// by binary search instead of scanning the whole document on every frame.
fn resolve_styles(
    highlights: &[(Range<usize>, &'static str)],
    range: &Range<usize>,
    resolver: &dyn HighlightStyleResolver,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let first = highlights.partition_point(|(highlight, _)| highlight.end <= range.start);
    let mut runs = Vec::new();
    let mut cursor = range.start;

    for (highlight_range, name) in &highlights[first..] {
        if highlight_range.start >= range.end {
            break;
        }

        let start = highlight_range.start.max(range.start);
        let end = highlight_range.end.min(range.end);
        if start >= end || end <= cursor {
            continue;
        }
        if cursor < start {
            runs.push((cursor..start, HighlightStyle::default()));
        }
        runs.push((start..end, resolver.style(name).unwrap_or_default()));
        cursor = end;
    }

    if cursor < range.end {
        runs.push((cursor..range.end, HighlightStyle::default()));
    }
    runs
}

fn semantic_name(scope: Scope) -> Option<&'static str> {
    let scope = scope.build_string();
    let name = if scope.starts_with("comment") {
        "comment"
    } else if scope.starts_with("constant.character.escape") {
        "string.escape"
    } else if scope.starts_with("string") {
        "string"
    } else if scope.starts_with("constant.numeric") {
        "number"
    } else if scope.starts_with("constant.language.boolean") {
        "boolean"
    } else if scope.starts_with("keyword.operator") {
        "operator"
    } else if scope.starts_with("keyword") || scope.starts_with("storage") {
        "keyword"
    } else if scope.starts_with("entity.name.function") || scope.starts_with("support.function") {
        "function"
    } else if scope.starts_with("entity.name.type")
        || scope.starts_with("entity.name.class")
        || scope.starts_with("support.type")
    {
        "type"
    } else if scope.starts_with("variable") {
        "variable"
    } else if scope.starts_with("constant") {
        "constant"
    } else if scope.starts_with("punctuation") {
        "punctuation"
    } else {
        return None;
    };
    Some(name)
}

fn brace_fold_ranges(text: &str) -> Vec<FoldRange> {
    let mut starts = Vec::new();
    let mut ranges = Vec::new();

    for (line_number, line) in text.lines().enumerate() {
        let mut chars = line.chars().peekable();
        let mut quoted = false;
        let mut escaped = false;
        while let Some(character) = chars.next() {
            if !quoted && character == '/' && chars.peek() == Some(&'/') {
                break;
            }
            if character == '"' && !escaped {
                quoted = !quoted;
            } else if !quoted && character == '{' {
                starts.push(line_number);
            } else if !quoted && character == '}' {
                if let Some(start_line) = starts.pop() {
                    if start_line < line_number {
                        ranges.push(FoldRange::new(start_line, line_number));
                    }
                }
            }
            escaped = quoted && character == '\\' && !escaped;
            if character != '\\' {
                escaped = false;
            }
        }
    }
    ranges
}

/// HERDR: semantic overrides fall back to readable, dark-editor defaults.
#[derive(Default)]
pub(crate) struct SyntaxStyles(pub std::collections::HashMap<String, gpui::Hsla>);

impl HighlightStyleResolver for SyntaxStyles {
    fn style(&self, name: &str) -> Option<HighlightStyle> {
        let root = name.split('.').next()?;
        let color = self
            .0
            .get(name)
            .or_else(|| self.0.get(root))
            .copied()
            .or_else(|| {
                let rgb = match root {
                    "comment" => 0x7f848e,
                    "string" => 0x98c379,
                    "number" => 0xd19a66,
                    "keyword" => 0xc678dd,
                    "boolean" | "constant" => 0xe5c07b,
                    "function" => 0x61afef,
                    "type" => 0xe5c07b,
                    "operator" => 0x56b6c2,
                    // Unclassified identifiers and punctuation follow the theme foreground.
                    _ => return None,
                };
                Some(gpui::rgb(rgb).into())
            });
        color.map(|color| HighlightStyle {
            color: Some(color),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_styles_cover_the_requested_range_without_gaps() {
        let runs = resolve_styles(
            &[(2..4, "keyword"), (6..8, "string")],
            &(0..10),
            &SyntaxStyles::default(),
        );
        let ranges: Vec<_> = runs.into_iter().map(|(range, _)| range).collect();
        assert_eq!(ranges, vec![0..2, 2..4, 4..6, 6..8, 8..10]);
    }

    #[test]
    fn semantic_overrides_and_nested_names_resolve() {
        let color = gpui::rgb(0xc678dd).into();
        let styles = SyntaxStyles(HashMap::from([("string".into(), color)]));
        assert_eq!(styles.style("string.escape").unwrap().color, Some(color));
        assert!(styles.style("keyword").is_some());
        assert!(find_syntax("rust").is_some());
        assert!(find_syntax("rs").is_some());
        assert!(find_syntax("unknown-language").is_none());
    }

    /// The previous whole-document parse, kept as the reference result.
    fn full_highlights(text: &str, language: &str) -> Vec<(Range<usize>, &'static str)> {
        let mut parser = ParseState::new(find_syntax(language).unwrap());
        let mut scopes = ScopeStack::new();
        let mut names = SemanticNames::default();
        let (mut out, mut offset) = (Vec::new(), 0);
        for line in syntect::util::LinesWithEndings::from(text) {
            parse_line(line, offset, &mut parser, &mut scopes, &mut names, &mut out);
            offset += line.len();
        }
        out
    }

    fn row_of(text: &str, byte: usize) -> usize {
        text[..byte].bytes().filter(|byte| *byte == b'\n').count()
    }

    fn input_edit(old: &str, new: &str, start: usize, old_end: usize, new_end: usize) -> InputEdit {
        use gpui_base::input::Point;
        InputEdit {
            start_byte: start,
            old_end_byte: old_end,
            new_end_byte: new_end,
            start_position: Point::new(row_of(old, start), 0),
            old_end_position: Point::new(row_of(old, old_end), 0),
            new_end_position: Point::new(row_of(new, new_end), 0),
        }
    }

    #[test]
    fn incremental_updates_match_a_full_reparse() {
        const SAMPLES: &[(&str, &str)] = &[
            (
                "js",
                "// c\nconst a = 1;\n/* block\n  comment */\nfunction f(x) {\n  return `t ${x}` + \"s\" + 'q' + /r[a-z]/g;\n}\n",
            ),
            (
                "rs",
                "/// doc\nfn main() {\n    let s = r#\"raw \"q\"#;\n    /* a /* nested */ b */\n    let c = 'c';\n}\n",
            ),
            (
                "py",
                "def f(x):\n    \"\"\"doc\n    string\"\"\"\n    return f'{x}'  # c\n",
            ),
            ("md", "# T\n\n```js\nconst a = `x`;\n```\n- *em*\n"),
        ];
        const SNIPPETS: &[&str] = &[
            "x", "\n", "\"", "'", "`", "/*", "*/", "${", "}", "{", "// c\n", "日本", "\"\"\"",
            "r#\"", "```", "\r\n",
        ];
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = |bound: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % bound.max(1) as u64) as usize
        };
        for (language, sample) in SAMPLES {
            for _ in 0..20 {
                let mut text = sample.repeat(1 + next(3));
                let mut cache = LineCache::new(find_syntax(language).unwrap(), &text);
                for _ in 0..60 {
                    let mut start = next(text.len() + 1);
                    while !text.is_char_boundary(start) {
                        start -= 1;
                    }
                    let mut old_end = (start + next(10)).min(text.len());
                    while !text.is_char_boundary(old_end) {
                        old_end -= 1;
                    }
                    let insert: String = (0..next(3))
                        .map(|_| SNIPPETS[next(SNIPPETS.len())])
                        .collect();
                    let new = format!("{}{}{}", &text[..start], insert, &text[old_end..]);
                    let edit = input_edit(&text, &new, start, old_end, start + insert.len());
                    cache.apply(&edit, &new);
                    let all = cache.highlights_in(&(0..new.len() + 1));
                    assert_eq!(
                        all,
                        full_highlights(&new, language),
                        "{language}: {edit:?} in {new:?}"
                    );
                    text = new;
                }
            }
        }
    }

    #[test]
    fn a_local_edit_reparses_only_the_edited_line() {
        let line = "export function f(a: number): string { return `${a}`; } // c\n";
        let text = line.repeat(2000);
        let mut cache = LineCache::new(find_syntax("js").unwrap(), &text);
        let new = format!("{}x{}", &text[..20], &text[20..]);
        cache.apply(&input_edit(&text, &new, 20, 20, 21), &new);
        assert_eq!(cache.parsed_rows, 1);
        assert_eq!(
            cache.highlights_in(&(0..new.len())),
            full_highlights(&new, "js")
        );
    }
}
