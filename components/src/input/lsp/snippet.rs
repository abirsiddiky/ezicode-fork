//! Parser and navigation state for LSP snippets.
//!
//! Implements the snippet grammar from the LSP specification
//! (<https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#snippet_syntax>):
//!
//! - tabstops `$1`, `$2`, `$0`
//! - placeholders `${1:default}`, possibly nested
//! - choices `${1|a,b,c|}` (the first choice is used)
//! - variables `$NAME` / `${NAME}` / `${NAME:default}`
//! - escapes `\$`, `\}`, `\\`
//!
//! Everything in this module is plain data (byte offsets into `String`s), so it
//! can be unit tested without a window.

use std::ops::Range;

/// One tabstop occurrence in the expanded text. Offsets are byte offsets into
/// [`Snippet::text`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabStop {
    pub index: u32,
    pub range: Range<usize>,
}

/// A snippet with all markers removed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snippet {
    /// The final plain text, with no `$` markers left in it.
    pub text: String,
    /// Tabstops in navigation order: `1, 2, 3, ...` then `0` last.
    ///
    /// Each index appears once (its first occurrence, preferring a non-empty
    /// placeholder). A `$0` is always present: if the snippet has none, one is
    /// added at the end of the text.
    pub tabstops: Vec<TabStop>,
}

/// Parse an LSP snippet, resolving no variables.
///
/// Unknown variables expand to their name (or to their `${NAME:default}`
/// default when one is given).
pub fn parse(input: &str) -> Snippet {
    parse_with(input, &no_variables)
}

fn no_variables(_: &str) -> Option<String> {
    None
}

/// Parse an LSP snippet. `resolve` maps a variable name such as `TM_FILENAME`
/// to its value; returning `None` falls back to the default / the name.
///
/// Malformed markers never fail: a `$` or `${` that does not form a valid
/// marker is kept as literal text.
pub fn parse_with(input: &str, resolve: &dyn Fn(&str) -> Option<String>) -> Snippet {
    let chars: Vec<char> = input.chars().collect();
    let mut parser = Parser {
        chars: &chars,
        pos: 0,
        out: String::new(),
        stops: Vec::new(),
        resolve,
    };
    parser.parse_until(None);
    parser.finish()
}

#[derive(Clone, Copy)]
struct Snap {
    pos: usize,
    out_len: usize,
    stops_len: usize,
}

struct Parser<'a> {
    chars: &'a [char],
    pos: usize,
    out: String,
    /// Tabstops in the order they close in the text (not yet deduplicated).
    stops: Vec<TabStop>,
    resolve: &'a dyn Fn(&str) -> Option<String>,
}

impl Parser<'_> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, n: usize) -> Option<char> {
        self.chars.get(self.pos + n).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += 1;
        Some(c)
    }

    fn snapshot(&self) -> Snap {
        Snap {
            pos: self.pos,
            out_len: self.out.len(),
            stops_len: self.stops.len(),
        }
    }

    fn restore(&mut self, snap: Snap) {
        self.pos = snap.pos;
        self.out.truncate(snap.out_len);
        self.stops.truncate(snap.stops_len);
    }

    fn push_stop(&mut self, index: u32, range: Range<usize>) {
        self.stops.push(TabStop { index, range });
    }

    /// Parse plain text (with escapes and markers) up to the unescaped `close`
    /// character, which is left unconsumed. `None` parses to end of input.
    fn parse_until(&mut self, close: Option<char>) {
        while let Some(c) = self.peek() {
            if Some(c) == close {
                break;
            }
            match c {
                '\\' => match self.peek_at(1) {
                    Some(escaped @ ('$' | '}' | '\\')) => {
                        self.out.push(escaped);
                        self.pos += 2;
                    }
                    // A backslash before anything else is just a backslash.
                    _ => {
                        self.out.push('\\');
                        self.pos += 1;
                    }
                },
                '$' => {
                    let snap = self.snapshot();
                    self.pos += 1;
                    if !self.parse_after_dollar() {
                        // Not a valid marker: keep the `$` as text and carry on.
                        self.restore(snap);
                        self.out.push('$');
                        self.pos += 1;
                    }
                }
                _ => {
                    self.out.push(c);
                    self.pos += 1;
                }
            }
        }
    }

    /// Called with the `$` already consumed. Returns `false` if this is not a
    /// valid marker (the caller then restores its snapshot).
    fn parse_after_dollar(&mut self) -> bool {
        match self.peek() {
            Some('{') => {
                self.pos += 1;
                self.parse_braced()
            }
            Some(c) if c.is_ascii_digit() => {
                let index = self.read_int();
                let at = self.out.len();
                self.push_stop(index, at..at);
                true
            }
            Some(c) if is_name_start(c) => {
                let name = self.read_name();
                self.insert_variable(&name);
                true
            }
            _ => false,
        }
    }

    /// Called after `${` has been consumed.
    fn parse_braced(&mut self) -> bool {
        match self.peek() {
            Some(c) if c.is_ascii_digit() => {
                let index = self.read_int();
                match self.bump() {
                    // ${1}
                    Some('}') => {
                        let at = self.out.len();
                        self.push_stop(index, at..at);
                        true
                    }
                    // ${1:placeholder}
                    Some(':') => {
                        let start = self.out.len();
                        self.parse_until(Some('}'));
                        if self.bump() != Some('}') {
                            return false;
                        }
                        self.push_stop(index, start..self.out.len());
                        true
                    }
                    // ${1|a,b,c|}
                    Some('|') => {
                        let Some(choice) = self.read_choices() else {
                            return false;
                        };
                        let start = self.out.len();
                        self.out.push_str(&choice);
                        self.push_stop(index, start..self.out.len());
                        true
                    }
                    _ => false,
                }
            }
            Some(c) if is_name_start(c) => {
                let name = self.read_name();
                match self.bump() {
                    // ${NAME}
                    Some('}') => {
                        self.insert_variable(&name);
                        true
                    }
                    // ${NAME:default}
                    Some(':') => {
                        let start = self.out.len();
                        let stops_len = self.stops.len();
                        self.parse_until(Some('}'));
                        if self.bump() != Some('}') {
                            return false;
                        }
                        if let Some(value) = (self.resolve)(name.as_str()) {
                            // The variable exists, so its default is discarded
                            // (including any tabstops parsed inside it).
                            self.out.truncate(start);
                            self.stops.truncate(stops_len);
                            self.out.push_str(&value);
                        }
                        true
                    }
                    _ => false,
                }
            }
            _ => false,
        }
    }

    /// Insert a variable's value, or its name if it cannot be resolved.
    fn insert_variable(&mut self, name: &str) {
        match (self.resolve)(name) {
            Some(value) => self.out.push_str(&value),
            None => self.out.push_str(name),
        }
    }

    /// Reads `first|...|}` after the opening `|`, returning the first choice.
    /// Consumes the closing `|}`.
    fn read_choices(&mut self) -> Option<String> {
        let mut first = String::new();
        let mut in_first = true;
        loop {
            match self.bump()? {
                '\\' => match self.peek() {
                    Some(escaped @ (',' | '|' | '\\')) => {
                        self.pos += 1;
                        if in_first {
                            first.push(escaped);
                        }
                    }
                    _ => {
                        if in_first {
                            first.push('\\');
                        }
                    }
                },
                ',' => in_first = false,
                '|' => break,
                c => {
                    if in_first {
                        first.push(c);
                    }
                }
            }
        }
        (self.bump()? == '}').then_some(first)
    }

    fn read_int(&mut self) -> u32 {
        let mut value: u32 = 0;
        while let Some(d) = self.peek().and_then(|c| c.to_digit(10)) {
            value = value.saturating_mul(10).saturating_add(d);
            self.pos += 1;
        }
        value
    }

    fn read_name(&mut self) -> String {
        let mut name = String::new();
        while let Some(c) = self.peek().filter(|c| is_name_char(*c)) {
            name.push(c);
            self.pos += 1;
        }
        name
    }

    fn finish(self) -> Snippet {
        let mut stops: Vec<TabStop> = Vec::new();
        for stop in self.stops {
            match stops.iter().position(|s| s.index == stop.index) {
                // Prefer a placeholder over an empty mirror like a bare `$1`.
                Some(i) => {
                    if stops[i].range.is_empty() && !stop.range.is_empty() {
                        stops[i].range = stop.range;
                    }
                }
                None => stops.push(stop),
            }
        }

        let end = self.out.len();
        if !stops.iter().any(|s| s.index == 0) {
            stops.push(TabStop {
                index: 0,
                range: end..end,
            });
        }
        // Numbered stops ascending, then `$0` last.
        stops.sort_by_key(|s| (s.index == 0, s.index));

        Snippet {
            text: self.out,
            tabstops: stops,
        }
    }
}

fn is_name_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// A snippet expanded into the buffer, tracking where its tabstops are while
/// the user edits.
///
/// All offsets are byte offsets into the buffer text.
#[derive(Clone, Debug)]
pub struct SnippetSession {
    /// The span of the inserted snippet text.
    range: Range<usize>,
    /// Tabstops in navigation order; never empty.
    stops: Vec<Range<usize>>,
    current: usize,
}

impl SnippetSession {
    /// Start a session for `snippet`, which was inserted at byte offset `base`.
    pub fn new(snippet: &Snippet, base: usize) -> Self {
        let stops = snippet
            .tabstops
            .iter()
            .map(|stop| base + stop.range.start..base + stop.range.end)
            .collect::<Vec<_>>();
        debug_assert!(!stops.is_empty(), "parse() always adds a $0 tabstop");

        Self {
            range: base..base + snippet.text.len(),
            stops,
            current: 0,
        }
    }

    /// The range of the tabstop currently selected.
    pub fn current_range(&self) -> Range<usize> {
        self.stops[self.current].clone()
    }

    /// Advance to the next tabstop. `None` when already on the last one (the
    /// `$0`), which means the session is finished.
    pub fn next(&mut self) -> Option<Range<usize>> {
        if self.current + 1 < self.stops.len() {
            self.current += 1;
            Some(self.current_range())
        } else {
            None
        }
    }

    /// Go back to the previous tabstop. `None` when already on the first one.
    pub fn prev(&mut self) -> Option<Range<usize>> {
        if self.current > 0 {
            self.current -= 1;
            Some(self.current_range())
        } else {
            None
        }
    }

    /// Whether `offset` is inside the inserted snippet (the ends count).
    pub fn contains(&self, offset: usize) -> bool {
        self.range.start <= offset && offset <= self.range.end
    }

    /// Update all ranges after `edit` (a byte range in the old text) was
    /// replaced with `new_len` bytes.
    pub fn on_edit(&mut self, edit: Range<usize>, new_len: usize) {
        let start = map_start(self.range.start, &edit, new_len);
        let end = map_end(self.range.end, &edit, new_len);
        self.range = start..end.max(start);

        for stop in &mut self.stops {
            let s = map_start(stop.start, &edit, new_len);
            let e = map_end(stop.end, &edit, new_len);
            *stop = s..e.max(s);
        }
    }
}

/// Where a position that was at `p` (as a range start) ends up after an edit.
/// Text typed at the start of a tabstop goes into that tabstop.
fn map_start(p: usize, edit: &Range<usize>, new_len: usize) -> usize {
    if p <= edit.start {
        p
    } else if p >= edit.end {
        shift(p, edit, new_len)
    } else {
        edit.start
    }
}

/// Where a position that was at `p` (as a range end) ends up after an edit.
/// Text typed at the end of a tabstop goes into that tabstop.
fn map_end(p: usize, edit: &Range<usize>, new_len: usize) -> usize {
    if p < edit.start {
        p
    } else if p >= edit.end {
        shift(p, edit, new_len)
    } else {
        edit.start + new_len
    }
}

fn shift(p: usize, edit: &Range<usize>, new_len: usize) -> usize {
    // Callers guarantee `p >= edit.end`, so this cannot underflow.
    p - edit.end + edit.start + new_len
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stops(snippet: &Snippet) -> Vec<(u32, Range<usize>)> {
        snippet
            .tabstops
            .iter()
            .map(|s| (s.index, s.range.clone()))
            .collect()
    }

    #[test]
    fn plain_text_has_only_a_final_stop_at_the_end() {
        let s = parse("hello");
        assert_eq!(s.text, "hello");
        assert_eq!(stops(&s), vec![(0, 5..5)]);
    }

    #[test]
    fn final_tabstop_is_removed_and_marks_the_cursor() {
        let s = parse("<h1>$0</h1>");
        assert_eq!(s.text, "<h1></h1>");
        assert_eq!(stops(&s), vec![(0, 4..4)]);
    }

    #[test]
    fn final_tabstop_is_last_even_when_written_first() {
        let s = parse("$0 and $1");
        assert_eq!(s.text, " and ");
        assert_eq!(stops(&s), vec![(1, 5..5), (0, 0..0)]);
    }

    #[test]
    fn placeholder_text_is_kept_and_its_range_recorded() {
        let s = parse("<div class=\"${1:box}\">$0</div>");
        assert_eq!(s.text, "<div class=\"box\"></div>");
        assert_eq!(stops(&s), vec![(1, 12..15), (0, 17..17)]);
        assert_eq!(&s.text[12..15], "box");
    }

    #[test]
    fn nested_placeholders_produce_nested_ranges() {
        let s = parse("${1:outer ${2:inner} end}$0");
        assert_eq!(s.text, "outer inner end");
        assert_eq!(stops(&s), vec![(1, 0..15), (2, 6..11), (0, 15..15)]);
        assert_eq!(&s.text[6..11], "inner");
    }

    #[test]
    fn escaped_dollar_is_literal_and_not_a_tabstop() {
        let s = parse(r"cost \$1 and \$0");
        assert_eq!(s.text, "cost $1 and $0");
        assert_eq!(stops(&s), vec![(0, 14..14)]);
    }

    #[test]
    fn escaped_brace_and_backslash_inside_placeholder() {
        let s = parse(r"${1:a\}b\\c}$0");
        assert_eq!(s.text, r"a}b\c");
        assert_eq!(stops(&s), vec![(1, 0..5), (0, 5..5)]);
    }

    #[test]
    fn backslash_before_ordinary_char_is_kept() {
        assert_eq!(parse(r"a\nb").text, r"a\nb");
    }

    #[test]
    fn multiple_tabstops_are_ordered_numerically() {
        // Written out of order on purpose; `$10` must sort after `$2`.
        let s = parse("$2 $10 $1 $0");
        assert_eq!(s.text, "   ");
        assert_eq!(stops(&s), vec![(1, 2..2), (2, 0..0), (10, 1..1), (0, 3..3)]);
    }

    #[test]
    fn repeated_tabstop_uses_first_occurrence_and_prefers_placeholder() {
        let s = parse("$1 ${1:name} $1$0");
        assert_eq!(s.text, " name ");
        assert_eq!(stops(&s), vec![(1, 1..5), (0, 6..6)]);

        let s = parse("${1:name} $1");
        assert_eq!(stops(&s), vec![(1, 0..4), (0, 5..5)]);
    }

    #[test]
    fn choice_uses_the_first_option() {
        let s = parse("${1|a,b,c|}$0");
        assert_eq!(s.text, "a");
        assert_eq!(stops(&s), vec![(1, 0..1), (0, 1..1)]);
    }

    #[test]
    fn choice_escapes_are_honoured() {
        let s = parse(r"${1|x\,y,z|}");
        assert_eq!(s.text, "x,y");
    }

    #[test]
    fn variables_fall_back_to_their_name() {
        assert_eq!(parse("$TM_FILENAME").text, "TM_FILENAME");
        assert_eq!(parse("${TM_FILENAME}").text, "TM_FILENAME");
        assert_eq!(parse("${CLIPBOARD:paste}").text, "paste");
    }

    #[test]
    fn variables_use_the_resolver_when_it_knows_them() {
        let resolve = |name: &str| match name {
            "TM_FILENAME" => Some("index.html".to_string()),
            "CLIPBOARD" => Some(String::new()),
            _ => None,
        };
        let s = parse_with("<!-- $TM_FILENAME -->${CLIPBOARD:x}|$0", &resolve);
        assert_eq!(s.text, "<!-- index.html -->|");
    }

    #[test]
    fn unresolved_variable_default_is_discarded_when_resolved() {
        let resolve = |name: &str| (name == "V").then(|| "v".to_string());
        let s = parse_with("${V:${1:skip}}", &resolve);
        assert_eq!(s.text, "v");
        assert_eq!(stops(&s), vec![(0, 1..1)]);
    }

    #[test]
    fn malformed_markers_stay_literal() {
        assert_eq!(parse("cost: $").text, "cost: $");
        assert_eq!(parse("a ${ b").text, "a ${ b");
        assert_eq!(parse("${1:open").text, "${1:open");
        assert_eq!(parse("}").text, "}");
    }

    #[test]
    fn emmet_style_output_has_no_markers_left() {
        let s = parse("<ul>\n\t<li>${1}</li>\n\t<li>${2}</li>\n</ul>$0");
        assert_eq!(s.text, "<ul>\n\t<li></li>\n\t<li></li>\n</ul>");
        assert!(!s.text.contains('$'));
        assert_eq!(s.tabstops.len(), 3);
    }

    #[test]
    fn session_walks_tabstops_forward_and_back() {
        let s = parse("${1:a} ${2:b}$0");
        let mut session = SnippetSession::new(&s, 10);
        assert_eq!(session.current_range(), 10..11);
        assert_eq!(session.next(), Some(12..13));
        assert_eq!(session.prev(), Some(10..11));
        assert_eq!(session.prev(), None);
        assert_eq!(session.next(), Some(12..13));
        assert_eq!(session.next(), Some(13..13));
        assert_eq!(session.next(), None);
    }

    #[test]
    fn session_contains_is_inclusive_of_the_ends() {
        let s = parse("abc$0");
        let session = SnippetSession::new(&s, 5);
        assert!(session.contains(5));
        assert!(session.contains(8));
        assert!(!session.contains(4));
        assert!(!session.contains(9));
    }

    #[test]
    fn typing_inside_a_tabstop_grows_only_that_tabstop() {
        // Text `<div class="box"></div>`: `box` at 12..15, $0 at 17, length 23.
        let s = parse("<div class=\"${1:box}\">$0</div>");
        let mut session = SnippetSession::new(&s, 0);
        // Type "XY" in the middle of "box" (offset 13).
        session.on_edit(13..13, 2);
        assert_eq!(session.current_range(), 12..17);
        assert_eq!(session.stops[1], 19..19);
        assert_eq!(session.range, 0..25);
    }

    #[test]
    fn typing_before_the_session_shifts_it() {
        let s = parse("${1:a}$0");
        let mut session = SnippetSession::new(&s, 10);
        session.on_edit(0..0, 4);
        assert_eq!(session.range, 14..15);
        assert_eq!(session.current_range(), 14..15);
    }

    #[test]
    fn deleting_across_a_tabstop_boundary_keeps_ranges_valid() {
        let s = parse("${1:abc}-${2:def}$0");
        let mut session = SnippetSession::new(&s, 0);
        // Delete "c-d" (offsets 2..5); "ef" is now at 2..4.
        session.on_edit(2..5, 0);
        assert_eq!(session.stops[0], 0..2);
        assert_eq!(session.stops[1], 2..4);
        assert!(session.stops.iter().all(|r| r.start <= r.end));
    }

    #[test]
    fn typing_at_the_start_of_a_tabstop_goes_into_it() {
        let s = parse("${1:abc}$0");
        let mut session = SnippetSession::new(&s, 0);
        session.on_edit(0..0, 1);
        assert_eq!(session.stops[0], 0..4);
    }
}
