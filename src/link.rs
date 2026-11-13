use std::fmt;
use std::path::{Path, PathBuf};

use crate::error::{Result, TunrError};

/// Conditional expression parsed from one non-empty line in a `.link` file.
///
/// Precedence: `&&` binds tighter than `||`, both left-associative.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkExpr {
    Url(String),
    And(Box<LinkExpr>, Box<LinkExpr>),
    Or(Box<LinkExpr>, Box<LinkExpr>),
}

impl LinkExpr {
    /// Leftmost leaf URL (used for initial queue labels before execution starts).
    pub fn first_url(&self) -> &str {
        match self {
            Self::Url(u) => u,
            Self::And(lhs, _) | Self::Or(lhs, _) => lhs.first_url(),
        }
    }

    /// Total number of leaf URLs in this expression (upper bound of sequential steps).
    pub fn leaf_count(&self) -> usize {
        match self {
            Self::Url(_) => 1,
            Self::And(lhs, rhs) | Self::Or(lhs, rhs) => lhs.leaf_count() + rhs.leaf_count(),
        }
    }
}

impl fmt::Display for LinkExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Url(u) => f.write_str(u),
            Self::And(lhs, rhs) => write!(f, "{lhs} && {rhs}"),
            Self::Or(lhs, rhs) => write!(f, "{lhs} || {rhs}"),
        }
    }
}

/// One executable top-level statement (from a `.link` file line or raw CLI URL).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkStatement {
    /// 1-based line number in the `.link` file (`None` for raw CLI URLs).
    pub line_number: Option<usize>,
    /// Source `.link` path (`None` for raw CLI URLs).
    pub source_file: Option<PathBuf>,
    pub expr: LinkExpr,
}

/// Positional target captured at CLI parse time (disk-free).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadTarget {
    Url(String),
    LinkFile(PathBuf),
}

/// Returns `true` when `raw` looks like a local `.link` file path rather than a URL.
///
/// Purely lexical and disk-free: matches non-empty values without `://` whose path extension
/// equals `"link"` (ASCII case-insensitive).
pub fn is_link_file_arg(raw: &str) -> bool {
    let trimmed = raw.trim();
    !trimmed.is_empty()
        && !trimmed.contains("://")
        && Path::new(trimmed)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("link"))
}

/// Strips a full-line `#` comment or a whitespace-preceded inline `#` comment.
///
/// `#` immediately following a non-whitespace character (such as URL `#fragment`s) is preserved.
pub fn strip_inline_comment(line: &str) -> &str {
    let mut prev_was_ws = true;
    for (byte_idx, ch) in line.char_indices() {
        if ch == '#' && prev_was_ws {
            return line[..byte_idx].trim();
        }
        prev_was_ws = ch.is_whitespace();
    }
    line.trim()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token<'a> {
    Atom(&'a str),
    And,
    Or,
}

fn format_line_error(source: Option<&Path>, line_num: usize, msg: impl fmt::Display) -> String {
    match source {
        Some(path) => format!("{}:{line_num}: {msg}", path.display()),
        None => format!("line {line_num}: {msg}"),
    }
}

fn tokenize_line<'a>(
    line: &'a str,
    source: Option<&Path>,
    line_num: usize,
) -> Result<Vec<Token<'a>>> {
    let bytes = line.as_bytes();
    let mut tokens = Vec::new();
    let mut atom_start: Option<usize> = None;
    let mut i = 0;

    while i < bytes.len() {
        let ch = line[i..].chars().next().expect("valid UTF-8 boundary");
        if ch.is_whitespace() {
            if let Some(start) = atom_start.take() {
                tokens.push(Token::Atom(&line[start..i]));
            }
            i += ch.len_utf8();
            continue;
        }

        match ch {
            '(' | ')' => {
                return Err(TunrError::InvalidArg(format_line_error(
                    source,
                    line_num,
                    "parentheses are not supported in .link expressions",
                )));
            }
            '&' => {
                let count = bytes[i..].iter().take_while(|&&b| b == b'&').count();
                if count >= 3 {
                    return Err(TunrError::InvalidArg(format_line_error(
                        source,
                        line_num,
                        "unexpected '&&&' (use '&&')",
                    )));
                }
                if count == 2 {
                    if let Some(start) = atom_start.take() {
                        tokens.push(Token::Atom(&line[start..i]));
                    }
                    tokens.push(Token::And);
                    i += 2;
                    continue;
                }
                // Single '&': allow inside a URL query string (e.g. `?a=1&b=2`),
                // reject when standing alone as an operator token.
                let next_is_sep = line[i + 1..]
                    .chars()
                    .next()
                    .is_none_or(|next| next.is_whitespace() || matches!(next, '|' | '(' | ')'));
                if atom_start.is_none() && next_is_sep {
                    return Err(TunrError::InvalidArg(format_line_error(
                        source,
                        line_num,
                        "unexpected single '&' (use '&&')",
                    )));
                }
                atom_start.get_or_insert(i);
                i += 1;
            }
            '|' => {
                let count = bytes[i..].iter().take_while(|&&b| b == b'|').count();
                if count >= 3 {
                    return Err(TunrError::InvalidArg(format_line_error(
                        source,
                        line_num,
                        "unexpected '|||' (use '||')",
                    )));
                }
                if count == 2 {
                    if let Some(start) = atom_start.take() {
                        tokens.push(Token::Atom(&line[start..i]));
                    }
                    tokens.push(Token::Or);
                    i += 2;
                    continue;
                }
                return Err(TunrError::InvalidArg(format_line_error(
                    source,
                    line_num,
                    "unexpected single '|' (use '||')",
                )));
            }
            _ => {
                atom_start.get_or_insert(i);
                i += ch.len_utf8();
            }
        }
    }

    if let Some(start) = atom_start {
        tokens.push(Token::Atom(&line[start..]));
    }

    Ok(tokens)
}

fn parse_primary(
    tokens: &[Token<'_>],
    pos: &mut usize,
    source: Option<&Path>,
    line_num: usize,
) -> Result<LinkExpr> {
    match tokens.get(*pos) {
        Some(Token::Atom(url)) => {
            *pos += 1;
            Ok(LinkExpr::Url((*url).to_string()))
        }
        Some(Token::And) => Err(TunrError::InvalidArg(format_line_error(
            source,
            line_num,
            "unexpected '&&'",
        ))),
        Some(Token::Or) => Err(TunrError::InvalidArg(format_line_error(
            source,
            line_num,
            "unexpected '||'",
        ))),
        None => Err(TunrError::InvalidArg(format_line_error(
            source,
            line_num,
            "expected URL after operator",
        ))),
    }
}

fn parse_and(
    tokens: &[Token<'_>],
    pos: &mut usize,
    source: Option<&Path>,
    line_num: usize,
) -> Result<LinkExpr> {
    let mut lhs = parse_primary(tokens, pos, source, line_num)?;
    while matches!(tokens.get(*pos), Some(Token::And)) {
        *pos += 1;
        let rhs = parse_primary(tokens, pos, source, line_num)?;
        lhs = LinkExpr::And(Box::new(lhs), Box::new(rhs));
    }
    Ok(lhs)
}

fn parse_or(
    tokens: &[Token<'_>],
    pos: &mut usize,
    source: Option<&Path>,
    line_num: usize,
) -> Result<LinkExpr> {
    let mut lhs = parse_and(tokens, pos, source, line_num)?;
    while matches!(tokens.get(*pos), Some(Token::Or)) {
        *pos += 1;
        let rhs = parse_and(tokens, pos, source, line_num)?;
        lhs = LinkExpr::Or(Box::new(lhs), Box::new(rhs));
    }
    Ok(lhs)
}

fn parse_line(line: &str, source: Option<&Path>, line_num: usize) -> Result<LinkExpr> {
    let tokens = tokenize_line(line, source, line_num)?;
    let mut pos = 0;
    let expr = parse_or(&tokens, &mut pos, source, line_num)?;
    if let Some(extra) = tokens.get(pos) {
        let msg = match extra {
            Token::Atom(tok) => {
                format!("unexpected token '{tok}' (missing '&&' or '||' operator)")
            }
            Token::And => "unexpected '&&'".to_string(),
            Token::Or => "unexpected '||'".to_string(),
        };
        return Err(TunrError::InvalidArg(format_line_error(
            source, line_num, msg,
        )));
    }
    Ok(expr)
}

/// Parses `.link` file content into ordered [`LinkStatement`]s.
///
/// Strips an optional UTF-8 BOM, supports `\n` and `\r\n` line endings, skips blank and comment
/// lines while preserving 1-based line numbers, and parses `&&` tighter than `||`.
pub fn parse_link_str(input: &str, source: Option<&Path>) -> Result<Vec<LinkStatement>> {
    let content = input.strip_prefix('\u{FEFF}').unwrap_or(input);
    let source_buf = source.map(Path::to_path_buf);
    let mut statements = Vec::new();

    for (idx, raw_line) in content.lines().enumerate() {
        let line_num = idx + 1;
        let stripped = strip_inline_comment(raw_line);
        if stripped.is_empty() {
            continue;
        }
        let expr = parse_line(stripped, source, line_num)?;
        statements.push(LinkStatement {
            line_number: Some(line_num),
            source_file: source_buf.clone(),
            expr,
        });
    }

    Ok(statements)
}

/// Reads and parses a `.link` file from disk, rejecting missing, non-file, or empty `.link` files.
pub fn parse_link_file(path: &Path) -> Result<Vec<LinkStatement>> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(TunrError::NotFound(format!(
                ".link file not found: {}",
                path.display()
            )));
        }
        Err(err) => return Err(TunrError::Io(err)),
    };

    if !meta.is_file() {
        return Err(TunrError::InvalidArg(format!(
            ".link path is not a file: {}",
            path.display()
        )));
    }

    let content = std::fs::read_to_string(path)?;
    let statements = parse_link_str(&content, Some(path))?;
    if statements.is_empty() {
        return Err(TunrError::InvalidArg(format!(
            ".link file contains no download URLs: {}",
            path.display()
        )));
    }

    Ok(statements)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TempDirGuard {
        path: PathBuf,
    }

    impl TempDirGuard {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "tunr-link-test-{label}-{}",
                uuid::Uuid::new_v4()
            ));
            fs::create_dir_all(&path).expect("create temp test dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn url(s: &str) -> LinkExpr {
        LinkExpr::Url(s.to_string())
    }

    fn and(lhs: LinkExpr, rhs: LinkExpr) -> LinkExpr {
        LinkExpr::And(Box::new(lhs), Box::new(rhs))
    }

    fn or(lhs: LinkExpr, rhs: LinkExpr) -> LinkExpr {
        LinkExpr::Or(Box::new(lhs), Box::new(rhs))
    }

    #[test]
    fn parses_test_links_sample_file() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-links/test.link");
        let statements = parse_link_file(&path).expect("parse test-links/test.link");
        assert_eq!(statements.len(), 4);

        assert_eq!(statements[0].line_number, Some(3));
        assert_eq!(statements[0].source_file.as_deref(), Some(path.as_path()));
        assert_eq!(statements[0].expr, url("https://google.com/example-link"));

        assert_eq!(statements[1].line_number, Some(4));
        assert_eq!(
            statements[1].expr,
            url("https://youtube.com/example-youtube-link")
        );

        assert_eq!(statements[2].line_number, Some(12));
        assert_eq!(
            statements[2].expr,
            or(
                url("https://failure.com/possible-failure"),
                url("https://success.com/will-succeed"),
            )
        );

        assert_eq!(statements[3].line_number, Some(13));
        assert_eq!(
            statements[3].expr,
            and(
                url("https://must-succeed.com"),
                url("https://run-next.com/this-will-download"),
            )
        );
    }

    #[test]
    fn link_expr_helpers_and_display() {
        let expr = or(
            and(url("https://a.com"), url("https://b.com")),
            url("https://c.com"),
        );
        assert_eq!(expr.first_url(), "https://a.com");
        assert_eq!(expr.leaf_count(), 3);
        assert_eq!(
            expr.to_string(),
            "https://a.com && https://b.com || https://c.com"
        );
    }

    #[test]
    fn precedence_and_left_associativity() {
        // `&&` binds tighter than `||`
        let parsed = parse_link_str("A || B && C", None).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].expr, or(url("A"), and(url("B"), url("C"))));

        let parsed = parse_link_str("A && B || C", None).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].expr, or(and(url("A"), url("B")), url("C")));

        // Left-associative chains
        let parsed = parse_link_str("A || B || C", None).unwrap();
        assert_eq!(parsed[0].expr, or(or(url("A"), url("B")), url("C")));

        let parsed = parse_link_str("A && B && C", None).unwrap();
        assert_eq!(parsed[0].expr, and(and(url("A"), url("B")), url("C")));

        let parsed = parse_link_str("A || B && C || D && E", None).unwrap();
        assert_eq!(
            parsed[0].expr,
            or(
                or(url("A"), and(url("B"), url("C"))),
                and(url("D"), url("E"))
            )
        );
    }

    #[test]
    fn inline_comments_vs_url_fragments() {
        assert_eq!(strip_inline_comment("# full line"), "");
        assert_eq!(strip_inline_comment("   # indented comment"), "");
        assert_eq!(strip_inline_comment("https://a.com # note"), "https://a.com");
        assert_eq!(
            strip_inline_comment("https://a.com#frag # note"),
            "https://a.com#frag"
        );
        assert_eq!(
            strip_inline_comment("https://a.com#frag\t# tab note"),
            "https://a.com#frag"
        );

        let parsed = parse_link_str(
            "https://a.com#t=30s || https://b.com#sec # fallback with fragments\n",
            None,
        )
        .unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(
            parsed[0].expr,
            or(url("https://a.com#t=30s"), url("https://b.com#sec"))
        );
    }

    #[test]
    fn bom_crlf_unspaced_operators_and_query_ampersands() {
        let input = "\u{FEFF}# header\r\n\r\nhttps://a.com/watch?v=1&list=2||https://b.com/watch?v=3&&https://c.com\r\n  https://d.com&&https://e.com  \r\n";
        let path = Path::new("lists/mix.link");
        let parsed = parse_link_str(input, Some(path)).unwrap();
        assert_eq!(parsed.len(), 2);

        assert_eq!(parsed[0].line_number, Some(3));
        assert_eq!(parsed[0].source_file.as_deref(), Some(path));
        assert_eq!(
            parsed[0].expr,
            or(
                url("https://a.com/watch?v=1&list=2"),
                and(url("https://b.com/watch?v=3"), url("https://c.com")),
            )
        );

        assert_eq!(parsed[1].line_number, Some(4));
        assert_eq!(
            parsed[1].expr,
            and(url("https://d.com"), url("https://e.com"))
        );
    }

    #[test]
    fn syntax_errors_report_file_and_line() {
        let src = Path::new("bad.link");
        let cases = [
            ("\n|| https://b.com", 2, "unexpected '||'"),
            ("&& https://b.com", 1, "unexpected '&&'"),
            ("https://a.com &&", 1, "expected URL after operator"),
            ("https://a.com ||", 1, "expected URL after operator"),
            ("https://a.com || && https://b.com", 1, "unexpected '&&'"),
            ("https://a.com && || https://b.com", 1, "unexpected '||'"),
            ("https://a.com https://b.com", 1, "missing '&&' or '||'"),
            ("https://a.com &&& https://b.com", 1, "unexpected '&&&'"),
            ("https://a.com ||| https://b.com", 1, "unexpected '|||'"),
            ("https://a.com & https://b.com", 1, "unexpected single '&'"),
            ("https://a.com | https://b.com", 1, "unexpected single '|'"),
            ("https://a.com|https://b.com", 1, "unexpected single '|'"),
            ("(https://a.com || https://b.com)", 1, "parentheses"),
            ("https://a.com && (https://b.com)", 1, "parentheses"),
        ];

        for (input, expected_line, expected_substr) in cases {
            let err = parse_link_str(input, Some(src)).unwrap_err();
            match err {
                TunrError::InvalidArg(msg) => {
                    let prefix = format!("bad.link:{expected_line}:");
                    assert!(
                        msg.contains(&prefix) && msg.contains(expected_substr),
                        "input {input:?} produced unexpected error: {msg}"
                    );
                }
                other => panic!("expected InvalidArg for {input:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn is_link_file_arg_cases() {
        assert!(is_link_file_arg("test.link"));
        assert!(is_link_file_arg("  ./test.link  "));
        assert!(is_link_file_arg("test-links/test.link"));
        assert!(is_link_file_arg("my lists/chill.link"));
        assert!(is_link_file_arg("TEST.LINK"));
        assert!(is_link_file_arg(r"C:\music\a.link"));

        assert!(!is_link_file_arg(""));
        assert!(!is_link_file_arg("   "));
        assert!(!is_link_file_arg(".link"));
        assert!(!is_link_file_arg("dir/.link"));
        assert!(!is_link_file_arg("https://example.com/foo.link"));
        assert!(!is_link_file_arg("file:///path/a.link"));
        assert!(!is_link_file_arg("playlist.txt"));
        assert!(!is_link_file_arg("link"));
    }

    #[test]
    fn parse_link_file_handles_valid_missing_dir_empty_and_invalid_utf8() {
        let dir = TempDirGuard::new("file-cases");

        let missing = dir.path().join("missing.link");
        assert!(matches!(
            parse_link_file(&missing),
            Err(TunrError::NotFound(_))
        ));

        let subdir = dir.path().join("dir.link");
        fs::create_dir_all(&subdir).unwrap();
        assert!(matches!(
            parse_link_file(&subdir),
            Err(TunrError::InvalidArg(_))
        ));

        let empty = dir.path().join("empty.link");
        fs::write(&empty, "# only comments\n\n   # and blanks\n").unwrap();
        assert!(matches!(
            parse_link_file(&empty),
            Err(TunrError::InvalidArg(_))
        ));

        let bad_utf8 = dir.path().join("bad-utf8.link");
        fs::write(&bad_utf8, [0xff_u8, 0xfe, 0x00]).unwrap();
        assert!(matches!(parse_link_file(&bad_utf8), Err(TunrError::Io(_))));

        let valid = dir.path().join("valid.link");
        fs::write(&valid, "https://a.com || https://b.com\n").unwrap();
        let stmts = parse_link_file(&valid).unwrap();
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0].source_file.as_deref(), Some(valid.as_path()));
        assert_eq!(stmts[0].line_number, Some(1));
        assert_eq!(stmts[0].expr, or(url("https://a.com"), url("https://b.com")));
    }
}
