//! Query parser: Everything-compatible subset (SPEC section 4).
//!
//! Grammar: whitespace = AND, `|` = OR, `!x` = NOT, `"phrase"`, `(group)` and
//! `<group>` grouping. A bare term with `*`/`?` is a whole-name glob,
//! otherwise a case-insensitive substring on the name. Known `prefix:` forms
//! are `ext:`, `path:`, `folder:`, `file:`, `regex:`, `case:` (stackable,
//! makes the term case-sensitive) and `wfn:` (whole filename). Unknown
//! `xxx:` prefixes are matched literally. [`parse`] never fails; a bad regex
//! falls back to a literal substring.
//!
//! Deliberate edge-case contracts (kept, not bugs):
//!
//! * An input of only operators (`"|"`, `"|||"`, `"()"`, `"<>"`) parses to
//!   [`Node::MatchAll`] (matches everything).
//! * Empty-valued functions (`"ext:"`, `"case:"`, `"path:"`, `"regex:"`,
//!   `"wfn:"`, bare `"folder:"`/`"file:"` keep only their type filter)
//!   contribute no constraint.
//! * An unclosed `"` is accepted (`"\"foo"` is `Substring("foo")`); the
//!   parser never fails.
//! * `regex:` compiles case-insensitively by prefixing `(?i)`; use
//!   `case:regex:` for a case-sensitive regex (an inline `(?-i)` also works
//!   because inner flags win, but `case:` is the supported switch).

use regex::Regex;

/// Parsed query: original text plus AST root.
#[derive(Debug, Clone)]
pub struct Query {
    /// The raw input string (used by the `prev`-narrowing rule).
    pub raw: String,
    /// AST root.
    pub root: Node,
}

impl Query {
    /// True when the AST contains an OR node.
    #[must_use]
    pub fn has_or(&self) -> bool {
        self.root.has_or()
    }

    /// True when the AST contains a NOT node.
    #[must_use]
    pub fn has_not(&self) -> bool {
        self.root.has_not()
    }

    /// True for an empty query (matches everything).
    #[must_use]
    pub fn is_match_all(&self) -> bool {
        matches!(self.root, Node::MatchAll)
    }
}

/// AST node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    /// Matches every live entry.
    MatchAll,
    /// All children must match.
    And(Vec<Node>),
    /// Any child must match.
    Or(Vec<Node>),
    /// Child must not match.
    Not(Box<Node>),
    /// Leaf term.
    Term(Term),
}

impl Node {
    fn has_or(&self) -> bool {
        match self {
            Node::Or(_) => true,
            Node::And(v) => v.iter().any(Node::has_or),
            Node::Not(n) => n.has_or(),
            _ => false,
        }
    }

    fn has_not(&self) -> bool {
        match self {
            Node::Not(_) => true,
            Node::And(v) | Node::Or(v) => v.iter().any(Node::has_not),
            _ => false,
        }
    }

    /// True when evaluation may need the rebuilt full path.
    #[must_use]
    pub fn uses_path(&self) -> bool {
        match self {
            Node::Term(t) => matches!(t.kind, TermKind::Path(_)),
            Node::And(v) | Node::Or(v) => v.iter().any(Node::uses_path),
            Node::Not(n) => n.uses_path(),
            Node::MatchAll => false,
        }
    }

    fn fold_and(items: Vec<Node>) -> Option<Node> {
        match items.len() {
            0 => None,
            1 => items.into_iter().next(),
            _ => Some(Node::And(items)),
        }
    }

    fn fold_or(arms: Vec<Node>) -> Option<Node> {
        match arms.len() {
            0 => None,
            1 => arms.into_iter().next(),
            _ => Some(Node::Or(arms)),
        }
    }
}

/// One leaf predicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    /// What to match.
    pub kind: TermKind,
    /// From a stacked `case:` prefix; ext/folder/file ignore it.
    pub case_sensitive: bool,
}

/// Leaf predicate shapes.
#[derive(Debug, Clone)]
pub enum TermKind {
    /// Substring on the file name.
    Substring(String),
    /// Whole-name glob (`*` = any run, `?` = one char).
    Glob(String),
    /// Whole-name equality.
    WholeName(String),
    /// Compiled regex on the file name (`pattern` kept for equality).
    Regex {
        /// Original pattern text.
        pattern: String,
        /// Compiled (with `(?i)` unless case-sensitive).
        compiled: Regex,
    },
    /// Extension list, already lowercased, no leading dots.
    Ext(Vec<String>),
    /// Substring on the rebuilt full path.
    Path(String),
    /// Direct child of the folder at this full path (`parent:`).
    Parent(String),
    /// `DIRECTORY` flag set.
    IsDir,
    /// `DIRECTORY` flag clear.
    IsFile,
}

impl PartialEq for TermKind {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (TermKind::Substring(a), TermKind::Substring(b))
            | (TermKind::Glob(a), TermKind::Glob(b))
            | (TermKind::WholeName(a), TermKind::WholeName(b))
            | (TermKind::Path(a), TermKind::Path(b))
            | (TermKind::Parent(a), TermKind::Parent(b)) => a == b,
            (TermKind::Regex { pattern: a, .. }, TermKind::Regex { pattern: b, .. }) => a == b,
            (TermKind::Ext(a), TermKind::Ext(b)) => a == b,
            (TermKind::IsDir, TermKind::IsDir) | (TermKind::IsFile, TermKind::IsFile) => true,
            _ => false,
        }
    }
}

impl Eq for TermKind {}

/// Parse query text. Never fails; empty input matches everything.
#[must_use]
pub fn parse(input: &str) -> Query {
    let mut p = Parser {
        chars: input.chars().collect(),
        pos: 0,
        depth: 0,
    };
    // A stray `)` or `>` ends `parse_or`; skip it and keep the rest of the
    // query instead of silently dropping it.
    let mut parts = Vec::new();
    loop {
        if let Some(n) = p.parse_or() {
            parts.push(n);
        }
        p.skip_ws();
        if p.eat().is_none() {
            break;
        }
    }
    Query {
        raw: input.to_string(),
        root: Node::fold_and(parts).unwrap_or(Node::MatchAll),
    }
}

/// Nesting limit for groups and `!`. The query comes from any pipe client and
/// each level is a stack frame in the elevated daemon; deeper operators are
/// dropped instead of overflowing the stack.
const MAX_DEPTH: u32 = 64;

struct Parser {
    chars: Vec<char>,
    pos: usize,
    depth: u32,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn eat(&mut self) -> Option<char> {
        let c = self.chars.get(self.pos).copied()?;
        self.pos += 1;
        Some(c)
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.pos += 1;
        }
    }

    fn parse_or(&mut self) -> Option<Node> {
        let mut arms = Vec::new();
        if let Some(n) = self.parse_and() {
            arms.push(n);
        }
        loop {
            self.skip_ws();
            if self.peek() == Some('|') {
                self.eat();
                if let Some(n) = self.parse_and() {
                    arms.push(n);
                }
            } else {
                break;
            }
        }
        Node::fold_or(arms)
    }

    fn parse_and(&mut self) -> Option<Node> {
        let mut items = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                None | Some('|') | Some(')') | Some('>') => break,
                _ => {}
            }
            let pos = self.pos;
            match self.parse_unary() {
                Some(n) => items.push(n),
                None if self.pos != pos => {}
                None => break,
            }
        }
        Node::fold_and(items)
    }

    fn parse_unary(&mut self) -> Option<Node> {
        self.skip_ws();
        if self.peek() == Some('!') {
            self.eat();
            if self.depth >= MAX_DEPTH {
                return None;
            }
            self.depth += 1;
            let inner = self.parse_unary();
            self.depth -= 1;
            return match inner {
                Some(n) => Some(Node::Not(Box::new(n))),
                None => plain_node("!", false),
            };
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Option<Node> {
        self.skip_ws();
        if matches!(self.peek(), Some('(' | '<')) && self.depth >= MAX_DEPTH {
            self.eat();
            return None;
        }
        match self.peek() {
            None | Some('|') | Some(')') | Some('>') => None,
            Some('(') => {
                self.eat();
                self.depth += 1;
                let n = self.parse_or();
                self.depth -= 1;
                self.skip_ws();
                if self.peek() == Some(')') {
                    self.eat();
                }
                n
            }
            Some('<') => {
                self.eat();
                self.depth += 1;
                let n = self.parse_or();
                self.depth -= 1;
                self.skip_ws();
                if self.peek() == Some('>') {
                    self.eat();
                }
                n
            }
            Some('"') => {
                self.eat();
                let mut s = String::new();
                loop {
                    match self.eat() {
                        None | Some('"') => break,
                        Some(c) => s.push(c),
                    }
                }
                Some(phrase_node(s))
            }
            Some(_) => {
                let mut s = String::new();
                // Inside `regex:` the pattern's own `(`, `)`, `|`, `<`, `>`
                // and `"` belong to it; only whitespace or a `)` that closes
                // an outer group ends it.
                let mut regex_parens: u32 = 0;
                while let Some(c) = self.peek() {
                    if c.is_whitespace() {
                        break;
                    }
                    if is_regex_term(&s) {
                        match c {
                            '(' => regex_parens += 1,
                            ')' if regex_parens == 0 => break,
                            ')' => regex_parens -= 1,
                            _ => {}
                        }
                    } else if matches!(c, '|' | '"' | '(' | ')' | '<' | '>') {
                        break;
                    }
                    s.push(c);
                    self.eat();
                }
                parse_bare(&s)
            }
        }
    }
}

fn phrase_node(s: String) -> Node {
    if s.is_empty() {
        Node::MatchAll
    } else {
        Node::Term(Term {
            kind: TermKind::Substring(s),
            case_sensitive: false,
        })
    }
}

fn plain_node(s: &str, case: bool) -> Option<Node> {
    if s.is_empty() {
        return None;
    }
    let kind = if s.contains(['*', '?']) {
        TermKind::Glob(s.to_string())
    } else {
        TermKind::Substring(s.to_string())
    };
    Some(Node::Term(Term {
        kind,
        case_sensitive: case,
    }))
}

fn strip_case_prefix(s: &str) -> Option<&str> {
    if s.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("case:")) {
        s.get(5..)
    } else {
        None
    }
}

fn parse_bare(s: &str) -> Option<Node> {
    let mut rest = s;
    let mut case = false;
    while let Some(after) = strip_case_prefix(rest) {
        case = true;
        rest = after;
    }
    if rest.is_empty() {
        return None; // bare `case:` carries no constraint
    }
    parse_func(rest, case)
}

fn dir_term() -> Term {
    Term {
        kind: TermKind::IsDir,
        case_sensitive: false,
    }
}

fn file_term() -> Term {
    Term {
        kind: TermKind::IsFile,
        case_sensitive: false,
    }
}

fn and_opt(a: Node, b: Option<Node>) -> Node {
    match b {
        None => a,
        Some(n) => Node::And(vec![a, n]),
    }
}

/// Extension lists behind Everything-style type macros (`audio:`, `pic:`...).
/// Sorted, as `ext:` lists are.
fn macro_exts(pre: &str) -> Option<&'static [&'static str]> {
    const AUDIO: &[&str] = &[
        "aac", "aif", "aiff", "ape", "flac", "m4a", "mid", "midi", "mp3", "ogg", "opus", "wav",
        "wma",
    ];
    const VIDEO: &[&str] = &[
        "3gp", "avi", "flv", "m2ts", "m4v", "mkv", "mov", "mp4", "mpeg", "mpg", "ts", "webm", "wmv",
    ];
    const DOC: &[&str] = &[
        "csv", "doc", "docx", "epub", "md", "odp", "ods", "odt", "pdf", "ppt", "pptx", "rtf",
        "txt", "xls", "xlsx",
    ];
    const PIC: &[&str] = &[
        "avif", "bmp", "gif", "heic", "ico", "jpeg", "jpg", "png", "psd", "raw", "svg", "tif",
        "tiff", "webp",
    ];
    const ZIP: &[&str] = &[
        "7z", "bz2", "cab", "gz", "iso", "rar", "tar", "tgz", "xz", "zip", "zst",
    ];
    const EXE: &[&str] = &["bat", "cmd", "com", "exe", "msi", "ps1", "scr"];
    let lists: [(&str, &'static [&'static str]); 7] = [
        ("audio", AUDIO),
        ("video", VIDEO),
        ("doc", DOC),
        ("pic", PIC),
        ("image", PIC),
        ("zip", ZIP),
        ("exe", EXE),
    ];
    lists
        .into_iter()
        .find(|(name, _)| pre.eq_ignore_ascii_case(name))
        .map(|(_, exts)| exts)
}

/// True once `s` reads `regex:` after any `case:`/`folder:`/`file:` prefixes.
fn is_regex_term(s: &str) -> bool {
    let mut rest = s;
    loop {
        let Some((pre, post)) = rest.split_once(':') else {
            return false;
        };
        if pre.eq_ignore_ascii_case("regex") {
            return true;
        }
        if !["case", "folder", "file"]
            .iter()
            .any(|p| pre.eq_ignore_ascii_case(p))
        {
            return false;
        }
        rest = post;
    }
}

fn parse_func(s: &str, case: bool) -> Option<Node> {
    if let Some(colon) = s.find(':') {
        let pre = &s[..colon];
        let post = &s[colon + 1..];
        if pre.eq_ignore_ascii_case("folder") {
            return Some(and_opt(
                Node::Term(dir_term()),
                if post.is_empty() {
                    None
                } else {
                    parse_func(post, case)
                },
            ));
        }
        if pre.eq_ignore_ascii_case("file") {
            return Some(and_opt(
                Node::Term(file_term()),
                if post.is_empty() {
                    None
                } else {
                    parse_func(post, case)
                },
            ));
        }
        if pre.eq_ignore_ascii_case("ext") {
            let mut list: Vec<String> = post
                .split([',', ';'])
                .map(|e| crate::fold::fold(e.trim().trim_start_matches('.')))
                .filter(|e| !e.is_empty())
                .collect();
            list.sort();
            list.dedup();
            if list.is_empty() {
                return None;
            }
            return Some(Node::Term(Term {
                kind: TermKind::Ext(list),
                case_sensitive: false,
            }));
        }
        if pre.eq_ignore_ascii_case("regex") {
            if post.is_empty() {
                return None;
            }
            let pat = if case {
                post.to_string()
            } else {
                format!("(?i){post}")
            };
            return Some(match Regex::new(&pat) {
                Ok(compiled) => Node::Term(Term {
                    kind: TermKind::Regex {
                        pattern: post.to_string(),
                        compiled,
                    },
                    case_sensitive: case,
                }),
                Err(_) => Node::Term(Term {
                    kind: TermKind::Substring(post.to_string()),
                    case_sensitive: case,
                }),
            });
        }
        if pre.eq_ignore_ascii_case("path") {
            if post.is_empty() {
                return None;
            }
            return Some(Node::Term(Term {
                kind: TermKind::Path(post.to_string()),
                case_sensitive: case,
            }));
        }
        if pre.eq_ignore_ascii_case("wfn") {
            if post.is_empty() {
                return None;
            }
            let kind = if post.contains(['*', '?']) {
                TermKind::Glob(post.to_string())
            } else {
                TermKind::WholeName(post.to_string())
            };
            return Some(Node::Term(Term {
                kind,
                case_sensitive: case,
            }));
        }
        if pre.eq_ignore_ascii_case("case") {
            // Reached via `folder:case:x`-style recursion.
            return parse_func(post, true);
        }
        if let Some(exts) = macro_exts(pre) {
            let ext = Node::Term(Term {
                kind: TermKind::Ext(exts.iter().map(|e| (*e).to_owned()).collect()),
                case_sensitive: false,
            });
            return Some(and_opt(
                ext,
                if post.is_empty() {
                    None
                } else {
                    parse_func(post, case)
                },
            ));
        }
        if pre.eq_ignore_ascii_case("parent") {
            if post.is_empty() {
                return None;
            }
            return Some(Node::Term(Term {
                kind: TermKind::Parent(post.to_string()),
                case_sensitive: false,
            }));
        }
        // `d:` alone, or a path like `d:\music`: everything under that path.
        if pre.len() == 1 && pre.as_bytes()[0].is_ascii_alphabetic() {
            if post.is_empty() {
                return Some(Node::Term(Term {
                    kind: TermKind::Path(format!("{pre}:\\")),
                    case_sensitive: false,
                }));
            }
            if post.starts_with(['\\', '/']) {
                return Some(Node::Term(Term {
                    kind: TermKind::Path(s.replace('/', "\\")),
                    case_sensitive: case,
                }));
            }
        }
        // Unknown `xxx:` prefix: literal, exactly like Everything.
        return plain_node(s, case);
    }
    plain_node(s, case)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(q: &Query) -> &Term {
        match &q.root {
            Node::Term(t) => t,
            other => panic!("expected term, got {other:?}"),
        }
    }

    #[test]
    fn stray_closer_keeps_the_rest_of_the_query() {
        assert_eq!(parse("main) rs").root, parse("main rs").root);
        assert_eq!(parse("report>2024").root, parse("report 2024").root);
    }

    #[test]
    fn deep_nesting_does_not_overflow() {
        let deep = "(".repeat(100_000) + "x";
        let _ = parse(&deep);
        let bangs = "!".repeat(100_000) + "x";
        let _ = parse(&bangs);
    }

    #[test]
    fn regex_keeps_its_parens_and_bars() {
        for q in ["regex:^IMG_(\\d+)", "regex:(jpg|png)$", "case:regex:(A|B)"] {
            let parsed = parse(q);
            let t = term(&parsed);
            assert!(matches!(t.kind, TermKind::Regex { .. }), "{q}: {t:?}");
        }
        // A `)` closing an outer group still ends the pattern.
        assert_eq!(parse("(regex:a.c) foo").root, parse("regex:a.c foo").root);
    }

    #[test]
    fn empty_matches_all() {
        assert!(parse("").is_match_all());
        assert!(parse("   ").is_match_all());
    }

    #[test]
    fn and_or_not() {
        let q = parse("a b");
        assert!(matches!(q.root, Node::And(_)));
        let q = parse("a|b");
        assert!(matches!(q.root, Node::Or(_)));
        assert!(q.has_or() && !q.has_not());
        let q = parse("a !b");
        assert!(q.has_not());
        assert!(!q.has_or());
    }

    #[test]
    fn quotes_and_groups() {
        let q = parse("\"a b\"");
        assert!(
            matches!(&q.root, Node::Term(t) if matches!(&t.kind, TermKind::Substring(s) if s == "a b"))
        );
        assert!(matches!(parse("(a|b)").root, Node::Or(_)));
        assert!(matches!(parse("<a|b>").root, Node::Or(_)));
        let q = parse("!(a|b)");
        assert!(matches!(q.root, Node::Not(_)));
    }

    #[test]
    fn functions() {
        assert!(matches!(term(&parse("ext:rs;toml")).kind, TermKind::Ext(_)));
        assert!(matches!(term(&parse("path:src")).kind, TermKind::Path(_)));
        assert!(matches!(term(&parse("folder:")).kind, TermKind::IsDir));
        assert!(matches!(term(&parse("file:")).kind, TermKind::IsFile));
        assert!(matches!(
            term(&parse("regex:^a+$")).kind,
            TermKind::Regex { .. }
        ));
        assert!(matches!(
            term(&parse("wfn:foo.txt")).kind,
            TermKind::WholeName(_)
        ));
        assert!(matches!(term(&parse("*.rs")).kind, TermKind::Glob(_)));
        assert!(term(&parse("case:Foo")).case_sensitive);
        // bad regex falls back to literal
        assert!(matches!(
            term(&parse("regex:(unclosed")).kind,
            TermKind::Substring(_)
        ));
        // unknown prefix is literal
        let qq = parse("zzz:qqq");
        let t = term(&qq);
        assert!(matches!(&t.kind, TermKind::Substring(s) if s == "zzz:qqq"));
    }
}
