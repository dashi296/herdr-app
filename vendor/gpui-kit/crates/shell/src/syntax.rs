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
    /// Byte length of the text the cache describes.
    len: usize,
    starts: Vec<usize>,
    lines: Vec<CachedLine>,
    /// Lines parsed by the last update.
    #[cfg(test)]
    parsed_rows: usize,
}

impl LineCache {
    fn new(syntax: &'static SyntaxReference, text: &str) -> Self {
        let mut cache = Self {
            syntax,
            names: SemanticNames::default(),
            len: 0,
            starts: Vec::new(),
            lines: Vec::new(),
            #[cfg(test)]
            parsed_rows: 0,
        };
        cache.reparse_all(text);
        cache
    }

    fn reparse_all(&mut self, text: &str) {
        self.len = text.len();
        self.starts = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(index, _)| index + 1))
            .collect();
        #[cfg(test)]
        {
            self.parsed_rows = 0;
        }
        let mut parser = ParseState::new(self.syntax);
        let mut scopes = ScopeStack::new();
        let mut lines = Vec::with_capacity(self.starts.len());
        for row in 0..self.starts.len() {
            let end = self.starts.get(row + 1).copied().unwrap_or(text.len());
            lines.push(self.parse(&text[self.starts[row]..end], &mut parser, &mut scopes));
        }
        self.lines = lines;
    }

    /// Parses one line, returning it with the state it started from.
    fn parse(
        &mut self,
        line: &str,
        parser: &mut ParseState,
        scopes: &mut ScopeStack,
    ) -> CachedLine {
        let state = (parser.clone(), scopes.clone());
        let mut highlights = Vec::new();
        #[cfg(test)]
        {
            self.parsed_rows += 1;
        }
        if !line.is_empty() {
            parse_line(line, 0, parser, scopes, &mut self.names, &mut highlights);
        }
        CachedLine { state, highlights }
    }

    /// Whether `byte` lies on `row` of the cached (pre-edit) text.
    fn on_row(&self, row: usize, byte: usize) -> bool {
        self.starts[row] <= byte && self.starts.get(row + 1).is_none_or(|next| byte < *next)
    }

    /// Applies one edit; `text` is the document after it. Lines before the edit
    /// keep their state; the edited lines are reparsed, and later lines are
    /// reparsed only until the parser state converges with their cached state.
    /// An edit that does not match the cached text falls back to a full parse.
    fn apply(&mut self, edit: &InputEdit, text: &str) {
        let (start_row, old_end_row, new_end_row) = (
            edit.start_position.row,
            edit.old_end_position.row,
            edit.new_end_position.row,
        );
        let consistent = start_row <= old_end_row
            && start_row <= new_end_row
            && old_end_row < self.lines.len()
            && edit.start_byte <= edit.old_end_byte
            && edit.old_end_byte <= self.len
            && edit.start_byte <= edit.new_end_byte
            && text.len() + edit.old_end_byte == self.len + edit.new_end_byte
            && self.on_row(start_row, edit.start_byte)
            && self.on_row(old_end_row, edit.old_end_byte)
            && text.is_char_boundary(edit.start_byte)
            && text.is_char_boundary(edit.new_end_byte);
        if !consistent {
            return self.reparse_all(text);
        }

        // Starts of the rows the edit produced (after `start_row`, up to `new_end_row`).
        let first = self.starts[start_row];
        let inserted: Vec<usize> = text[first..edit.new_end_byte]
            .match_indices('\n')
            .map(|(index, _)| first + index + 1)
            .collect();
        if inserted.len() != new_end_row - start_row {
            return self.reparse_all(text);
        }
        let delta = edit.new_end_byte as isize - edit.old_end_byte as isize;
        // Old start of a later row, moved to where it is in the new text.
        let moved = |start: usize| start.checked_add_signed(delta);

        #[cfg(test)]
        {
            self.parsed_rows = 0;
        }
        let (mut parser, mut scopes) = self.lines[start_row].state.clone();
        let mut fresh = Vec::new();
        let new_row_starts: Vec<usize> = std::iter::once(first)
            .chain(inserted.iter().copied())
            .collect();
        for (index, start) in new_row_starts.iter().enumerate() {
            let end = match new_row_starts.get(index + 1) {
                Some(next) => *next,
                None => match self.starts.get(old_end_row + 1) {
                    Some(old) => match moved(*old) {
                        Some(next) => next,
                        None => return self.reparse_all(text),
                    },
                    None => text.len(),
                },
            };
            fresh.push(self.parse(&text[*start..end], &mut parser, &mut scopes));
        }

        // Reparse the old rows after the edit until the state matches the cached one.
        let mut converged = self.lines.len();
        for old_row in old_end_row + 1..self.lines.len() {
            let cached = &self.lines[old_row].state;
            if cached.0 == parser && cached.1 == scopes {
                converged = old_row;
                break;
            }
            let (Some(start), end) = (
                moved(self.starts[old_row]),
                self.starts.get(old_row + 1).map(|next| moved(*next)),
            ) else {
                return self.reparse_all(text);
            };
            let end = match end {
                Some(Some(end)) => end,
                Some(None) => return self.reparse_all(text),
                None => text.len(),
            };
            if start > end || end > text.len() || !text.is_char_boundary(start) {
                return self.reparse_all(text);
            }
            fresh.push(self.parse(&text[start..end], &mut parser, &mut scopes));
        }

        // Splice the reparsed rows in place and shift the starts of the rows after the edit.
        self.lines.splice(start_row..converged, fresh);
        for start in &mut self.starts[old_end_row + 1..] {
            *start = start.wrapping_add_signed(delta);
        }
        self.starts.splice(start_row + 1..old_end_row + 1, inserted);
        self.len = text.len();
        debug_assert_eq!(self.lines.len(), self.starts.len());
    }

    /// Absolute highlights overlapping `range`, ordered and non-overlapping.
    fn highlights_in(
        &self,
        range: &Range<usize>,
    ) -> impl Iterator<Item = (Range<usize>, &'static str)> + '_ {
        let first = self
            .starts
            .partition_point(|start| *start <= range.start)
            .saturating_sub(1);
        let range = range.clone();
        (first..self.lines.len())
            .take_while(move |row| self.starts[*row] < range.end)
            .flat_map(move |row| {
                let base = self.starts[row];
                self.lines[row]
                    .highlights
                    .iter()
                    .map(move |(highlight, name)| {
                        (base + highlight.start..base + highlight.end, *name)
                    })
            })
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
        match (&mut self.lines, edit) {
            (Some(lines), Some(edit)) => lines.apply(&edit, &text),
            _ => {
                let syntax = find_syntax(self.language.as_ref())
                    .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());
                self.lines = Some(LineCache::new(syntax, &text));
            }
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
        match &self.lines {
            Some(lines) => resolve_runs(lines.highlights_in(range), range, resolver),
            None => resolve_runs(std::iter::empty(), range, resolver),
        }
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
#[cfg(test)]
fn resolve_styles(
    highlights: &[(Range<usize>, &'static str)],
    range: &Range<usize>,
    resolver: &dyn HighlightStyleResolver,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let first = highlights.partition_point(|(highlight, _)| highlight.end <= range.start);
    resolve_runs(highlights[first..].iter().cloned(), range, resolver)
}

/// Turn ordered, non-overlapping highlights that start no earlier than the
/// line containing `range.start` into gap-free style runs covering `range`.
fn resolve_runs(
    highlights: impl Iterator<Item = (Range<usize>, &'static str)>,
    range: &Range<usize>,
    resolver: &dyn HighlightStyleResolver,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let mut runs = Vec::new();
    let mut cursor = range.start;

    for (highlight_range, name) in highlights {
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

    /// The previous whole-document `update`, kept verbatim as an independent reference.
    fn full_highlights(text: &str, language: &str) -> Vec<(Range<usize>, &'static str)> {
        let mut parser = ParseState::new(find_syntax(language).unwrap());
        let mut scopes = ScopeStack::new();
        let mut names: HashMap<Scope, Option<&'static str>> = HashMap::new();
        let mut highlights = Vec::new();
        let mut push = |range: Range<usize>, scopes: &ScopeStack, out: &mut Vec<_>| {
            if range.is_empty() {
                return;
            }
            let name =
                scopes.scopes.iter().rev().find_map(|scope| {
                    *names.entry(*scope).or_insert_with(|| semantic_name(*scope))
                });
            if let Some(name) = name {
                out.push((range, name));
            }
        };
        let mut offset = 0;
        for line in syntect::util::LinesWithEndings::from(text) {
            if let Ok(operations) = parser.parse_line(line, &SYNTAX_SET) {
                let mut cursor = 0;
                for (index, operation) in operations {
                    push(offset + cursor..offset + index, &scopes, &mut highlights);
                    let _ = scopes.apply(&operation);
                    cursor = index;
                }
                push(
                    offset + cursor..offset + line.len(),
                    &scopes,
                    &mut highlights,
                );
            }
            offset += line.len();
        }
        highlights
    }

    fn row_of(text: &str, byte: usize) -> usize {
        text[..byte].bytes().filter(|byte| *byte == b'\n').count()
    }

    /// The edit gpui-base's `replacement_input_edit` builds for replacing `old[start..old_end]`.
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

    /// Replaces `text[start..old_end]` with `insert`, updating the cache.
    fn replace(
        cache: &mut LineCache,
        text: &mut String,
        start: usize,
        old_end: usize,
        insert: &str,
    ) {
        let new = format!("{}{}{}", &text[..start], insert, &text[old_end..]);
        cache.apply(
            &input_edit(text, &new, start, old_end, start + insert.len()),
            &new,
        );
        *text = new;
    }

    /// The cache must give the same highlights and style runs as a full parse.
    fn assert_matches(cache: &LineCache, text: &str, language: &str, context: &str) {
        let expected = full_highlights(text, language);
        let all: Vec<_> = cache.highlights_in(&(0..text.len() + 1)).collect();
        assert_eq!(all, expected, "{context}");
        let styles = SyntaxStyles::default();
        let len = text.len();
        for range in [
            0..len,
            0..0,
            len / 3..len / 2,
            len.saturating_sub(7)..len,
            1.min(len)..len,
        ] {
            assert_eq!(
                resolve_runs(cache.highlights_in(&range), &range, &styles),
                resolve_styles(&expected, &range, &styles),
                "{context}: styles for {range:?}"
            );
        }
    }

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

    #[test]
    fn incremental_updates_match_a_full_reparse() {
        const SNIPPETS: &[&str] = &[
            "x", "\n", "\"", "'", "`", "/*", "*/", "${", "}", "{", "// c\n", "日本", "\"\"\"",
            "r#\"", "```", "\r\n", "\r",
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
                    let floor = |text: &str, mut i: usize| {
                        while !text.is_char_boundary(i) {
                            i -= 1;
                        }
                        i
                    };
                    let start = floor(&text, next(text.len() + 1));
                    // Mostly short deletions; sometimes long ones across many lines.
                    let span = if next(8) == 0 {
                        next(text.len() + 1)
                    } else {
                        next(10)
                    };
                    let old_end = floor(&text, (start + span).min(text.len()));
                    let insert: String = (0..next(3))
                        .map(|_| SNIPPETS[next(SNIPPETS.len())])
                        .collect();
                    replace(&mut cache, &mut text, start, old_end, &insert);
                    assert_matches(&cache, &text, language, &format!("{language}: {text:?}"));
                }
            }
        }
    }

    #[test]
    fn boundary_edits_match_a_full_reparse() {
        let js = SAMPLES[0].1;
        let crlf = js.replace('\n', "\r\n");
        let cases: Vec<(&str, String, Box<dyn Fn(&str) -> (usize, usize, String)>)> = vec![
            (
                "empty to text",
                String::new(),
                Box::new(|_| (0, 0, js.to_owned())),
            ),
            (
                "text to empty",
                js.to_owned(),
                Box::new(|t| (0, t.len(), String::new())),
            ),
            (
                "replace everything",
                js.to_owned(),
                Box::new(|t| (0, t.len(), "let b = \"x\";\n/*".to_owned())),
            ),
            (
                "append at EOF",
                js.to_owned(),
                Box::new(|t| (t.len(), t.len(), "/* tail".to_owned())),
            ),
            (
                "delete at EOF",
                js.to_owned(),
                Box::new(|t| (t.len() - 3, t.len(), String::new())),
            ),
            (
                "remove final newline",
                js.to_owned(),
                Box::new(|t| (t.len() - 1, t.len(), String::new())),
            ),
            (
                "delete many lines",
                js.to_owned(),
                Box::new(|t| (3, t.len() - 5, String::new())),
            ),
            (
                "insert many lines",
                js.to_owned(),
                Box::new(|_| (10, 10, "\n/*\n\n*/\n`\n${\n}`\n".to_owned())),
            ),
            (
                "insert between CR and LF",
                crlf.clone(),
                Box::new(|t| {
                    let i = t.find("\r\n").unwrap() + 1;
                    (i, i, "x\n".to_owned())
                }),
            ),
            (
                "delete a CRLF",
                crlf.clone(),
                Box::new(|t| {
                    let i = t.find("\r\n").unwrap();
                    (i, i + 2, String::new())
                }),
            ),
            (
                "delete the LF of a CRLF",
                crlf,
                Box::new(|t| {
                    let i = t.find("\r\n").unwrap() + 1;
                    (i, i + 1, String::new())
                }),
            ),
        ];
        for (name, mut text, edit) in cases {
            let mut cache = LineCache::new(find_syntax("js").unwrap(), &text);
            let (start, old_end, insert) = edit(&text);
            replace(&mut cache, &mut text, start, old_end, &insert);
            assert_matches(&cache, &text, "js", name);
        }
    }

    #[test]
    fn batched_edits_apply_in_order() {
        // Multi-cursor typing: edits from the last cursor to the first, each with the text after it.
        let mut text = "a\nb\nc\nd\n".repeat(3);
        let mut cache = LineCache::new(find_syntax("js").unwrap(), &text);
        let cursors: Vec<usize> = text.match_indices('\n').map(|(i, _)| i).collect();
        for at in cursors.into_iter().rev() {
            replace(&mut cache, &mut text, at, at, "/*");
        }
        assert_matches(&cache, &text, "js", "batched");
    }

    #[test]
    fn an_edit_that_does_not_match_the_cache_reparses_everything() {
        use gpui_base::input::Point;
        let text = SAMPLES[0].1.to_owned();
        let edits = [
            // Rows that do not exist.
            InputEdit {
                start_byte: 0,
                old_end_byte: 0,
                new_end_byte: 0,
                start_position: Point::new(99, 0),
                old_end_position: Point::new(99, 0),
                new_end_position: Point::new(99, 0),
            },
            // New end row before the start row.
            InputEdit {
                start_byte: 5,
                old_end_byte: 5,
                new_end_byte: 6,
                start_position: Point::new(1, 0),
                old_end_position: Point::new(1, 0),
                new_end_position: Point::new(0, 0),
            },
            // Lengths that do not add up.
            InputEdit {
                start_byte: 0,
                old_end_byte: 1,
                new_end_byte: 9,
                start_position: Point::new(0, 0),
                old_end_position: Point::new(0, 0),
                new_end_position: Point::new(0, 0),
            },
        ];
        for edit in edits {
            let mut cache = LineCache::new(find_syntax("js").unwrap(), &text);
            let changed = format!("é{text}");
            cache.apply(&edit, &changed);
            assert_matches(&cache, &changed, "js", &format!("{edit:?}"));
        }
    }

    #[test]
    fn a_local_edit_reparses_only_the_edited_line() {
        let line = "export function f(a: number): string { return `${a}`; } // c\n";
        let mut text = line.repeat(2000);
        let mut cache = LineCache::new(find_syntax("js").unwrap(), &text);
        replace(&mut cache, &mut text, 20, 20, "x");
        assert_eq!(cache.parsed_rows, 1);
        assert_matches(&cache, &text, "js", "local edit");
    }
}
