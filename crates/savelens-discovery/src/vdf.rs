//! Parser for Valve's text KeyValues format (VDF / ACF files).
//!
//! Supported syntax:
//! * quoted (`"key"`) and unquoted (`key`) tokens,
//! * escape sequences `\\`, `\"`, `\n`, `\t` inside quoted tokens (any other backslash is kept
//!   literally, which matches how Steam writes unescaped Windows paths in old files),
//! * `//` line comments,
//! * nested `{ … }` blocks,
//! * `#include` / `#base` directives (ignored, reported as warnings),
//! * conditional suffixes such as `[$WIN32]` / `[!$X360]` (ignored).

use crate::DiscoveryError;

/// A node of a parsed KeyValues document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VdfNode {
    /// A leaf string value.
    Value(String),
    /// An ordered list of `(key, node)` pairs. Duplicate keys are preserved.
    Map(Vec<(String, VdfNode)>),
}

impl VdfNode {
    /// Returns the first child whose key matches `key` (ASCII case-insensitive).
    pub fn get(&self, key: &str) -> Option<&VdfNode> {
        match self {
            VdfNode::Map(entries) => entries
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v),
            VdfNode::Value(_) => None,
        }
    }

    /// Follows a chain of keys (each matched case-insensitively).
    pub fn get_path(&self, keys: &[&str]) -> Option<&VdfNode> {
        let mut node = self;
        for key in keys {
            node = node.get(key)?;
        }
        Some(node)
    }

    /// Convenience: `get(key)` followed by `as_str()`.
    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(VdfNode::as_str)
    }

    /// The string value of a leaf node.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            VdfNode::Value(v) => Some(v),
            VdfNode::Map(_) => None,
        }
    }

    /// The entries of a map node.
    pub fn as_map(&self) -> Option<&[(String, VdfNode)]> {
        match self {
            VdfNode::Map(entries) => Some(entries),
            VdfNode::Value(_) => None,
        }
    }
}

/// Parses a KeyValues document. The result is always a [`VdfNode::Map`] holding the top-level
/// entries. Ignored directives are logged through `tracing`.
pub fn parse(text: &str) -> Result<VdfNode, DiscoveryError> {
    let (node, warnings) = parse_with_warnings(text)?;
    for w in &warnings {
        tracing::warn!("{w}");
    }
    Ok(node)
}

/// Like [`parse`] but returns the warnings (ignored `#include`/`#base` directives) instead of
/// logging them.
pub fn parse_with_warnings(text: &str) -> Result<(VdfNode, Vec<String>), DiscoveryError> {
    let mut parser = Parser {
        lexer: Lexer::new(text),
        peeked: None,
        warnings: Vec::new(),
    };
    let entries = parser.parse_entries(None)?;
    Ok((VdfNode::Map(entries), parser.warnings))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TokenKind {
    Str { text: String, quoted: bool },
    Open,
    Close,
    Conditional,
    Eof,
}

#[derive(Debug, Clone)]
struct Token {
    kind: TokenKind,
    line: usize,
}

struct Lexer<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    line: usize,
}

fn err(line: usize, message: impl Into<String>) -> DiscoveryError {
    DiscoveryError::Vdf {
        line,
        message: message.into(),
    }
}

impl<'a> Lexer<'a> {
    fn new(text: &'a str) -> Self {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        Lexer {
            chars: text.chars().peekable(),
            line: 1,
        }
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.chars.next()?;
        if c == '\n' {
            self.line += 1;
        }
        Some(c)
    }

    fn skip_trivia(&mut self) {
        loop {
            match self.chars.peek().copied() {
                Some(c) if c.is_whitespace() => {
                    self.bump();
                }
                Some('/') => {
                    let mut look = self.chars.clone();
                    look.next();
                    if look.peek() == Some(&'/') {
                        while let Some(c) = self.chars.peek().copied() {
                            if c == '\n' {
                                break;
                            }
                            self.bump();
                        }
                    } else {
                        return;
                    }
                }
                _ => return,
            }
        }
    }

    fn next_token(&mut self) -> Result<Token, DiscoveryError> {
        self.skip_trivia();
        let line = self.line;
        let Some(c) = self.chars.peek().copied() else {
            return Ok(Token {
                kind: TokenKind::Eof,
                line,
            });
        };
        let kind = match c {
            '{' => {
                self.bump();
                TokenKind::Open
            }
            '}' => {
                self.bump();
                TokenKind::Close
            }
            '"' => {
                self.bump();
                TokenKind::Str {
                    text: self.quoted(line)?,
                    quoted: true,
                }
            }
            '[' => {
                self.bump();
                loop {
                    match self.bump() {
                        Some(']') => break,
                        Some('\n') | None => {
                            return Err(err(line, "unterminated conditional `[...]`"));
                        }
                        Some(_) => {}
                    }
                }
                TokenKind::Conditional
            }
            _ => {
                let mut text = String::new();
                while let Some(c) = self.chars.peek().copied() {
                    if c.is_whitespace() || c == '{' || c == '}' || c == '"' {
                        break;
                    }
                    if c == '/' {
                        let mut look = self.chars.clone();
                        look.next();
                        if look.peek() == Some(&'/') {
                            break;
                        }
                    }
                    text.push(c);
                    self.bump();
                }
                TokenKind::Str {
                    text,
                    quoted: false,
                }
            }
        };
        Ok(Token { kind, line })
    }

    fn quoted(&mut self, start_line: usize) -> Result<String, DiscoveryError> {
        let mut out = String::new();
        loop {
            match self.bump() {
                None => {
                    return Err(err(start_line, "unterminated quoted string"));
                }
                Some('"') => return Ok(out),
                Some('\\') => match self.chars.peek().copied() {
                    Some('\\') => {
                        self.bump();
                        out.push('\\');
                    }
                    Some('"') => {
                        self.bump();
                        out.push('"');
                    }
                    Some('n') => {
                        self.bump();
                        out.push('\n');
                    }
                    Some('t') => {
                        self.bump();
                        out.push('\t');
                    }
                    _ => out.push('\\'),
                },
                Some(c) => out.push(c),
            }
        }
    }
}

struct Parser<'a> {
    lexer: Lexer<'a>,
    peeked: Option<Token>,
    warnings: Vec<String>,
}

impl Parser<'_> {
    fn next(&mut self) -> Result<Token, DiscoveryError> {
        match self.peeked.take() {
            Some(t) => Ok(t),
            None => self.lexer.next_token(),
        }
    }

    fn peek(&mut self) -> Result<&Token, DiscoveryError> {
        if self.peeked.is_none() {
            self.peeked = Some(self.lexer.next_token()?);
        }
        match &self.peeked {
            Some(t) => Ok(t),
            None => Err(err(self.lexer.line, "internal lexer state error")),
        }
    }

    fn skip_conditional(&mut self) -> Result<(), DiscoveryError> {
        if self.peek()?.kind == TokenKind::Conditional {
            self.next()?;
        }
        Ok(())
    }

    /// Parses entries until EOF (`open_line == None`) or the matching `}`.
    fn parse_entries(
        &mut self,
        open_line: Option<usize>,
    ) -> Result<Vec<(String, VdfNode)>, DiscoveryError> {
        let mut entries = Vec::new();
        loop {
            let tok = self.next()?;
            let (key, quoted) = match tok.kind {
                TokenKind::Eof => {
                    return match open_line {
                        Some(l) => Err(err(
                            tok.line,
                            format!(
                                "unexpected end of input: block opened at line {l} is not closed"
                            ),
                        )),
                        None => Ok(entries),
                    };
                }
                TokenKind::Close => {
                    return match open_line {
                        Some(_) => Ok(entries),
                        None => Err(err(tok.line, "unexpected `}` without matching `{`")),
                    };
                }
                TokenKind::Open => return Err(err(tok.line, "expected a key, found `{`")),
                TokenKind::Conditional => continue,
                TokenKind::Str { text, quoted } => (text, quoted),
            };
            if !quoted {
                let lower = key.to_ascii_lowercase();
                if lower == "#include" || lower == "#base" {
                    let target = self.next()?;
                    let file = match target.kind {
                        TokenKind::Str { text, .. } => text,
                        _ => {
                            return Err(err(
                                target.line,
                                format!("`{key}` directive must be followed by a file name"),
                            ))
                        }
                    };
                    self.warnings.push(format!(
                        "line {}: ignored `{key} \"{file}\"` directive",
                        tok.line
                    ));
                    continue;
                }
            }
            self.skip_conditional()?;
            let value_tok = self.next()?;
            let node = match value_tok.kind {
                TokenKind::Str { text, .. } => VdfNode::Value(text),
                TokenKind::Open => VdfNode::Map(self.parse_entries(Some(value_tok.line))?),
                TokenKind::Eof => {
                    return Err(err(
                        value_tok.line,
                        format!("unexpected end of input: key `{key}` has no value"),
                    ))
                }
                TokenKind::Close => {
                    return Err(err(
                        value_tok.line,
                        format!("key `{key}` is followed by `}}` instead of a value"),
                    ))
                }
                TokenKind::Conditional => {
                    return Err(err(
                        value_tok.line,
                        format!("key `{key}` is followed by two conditionals"),
                    ))
                }
            };
            self.skip_conditional()?;
            entries.push((key, node));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_libraryfolders_format() {
        let text = r#"
"libraryfolders"
{
	"0"
	{
		"path"		"/home/u/.local/share/Steam"
		"label"		""
		"contentid"		"123"
		"apps"
		{
			"228980"		"29212173"
			"413150"		"1000"
		}
	}
	"1"
	{
		"path"		"/mnt/games/SteamLibrary"
		"label"		"Big Disk"
		"apps"
		{
			"1245620"		"5"
		}
	}
}
"#;
        let root = parse(text).unwrap();
        let lf = root.get("LibraryFolders").unwrap();
        let entries = lf.as_map().unwrap();
        assert_eq!(entries.len(), 2);
        let one = lf.get("1").unwrap();
        assert_eq!(one.get_str("path"), Some("/mnt/games/SteamLibrary"));
        assert_eq!(one.get_str("label"), Some("Big Disk"));
        let apps: Vec<&str> = lf
            .get_path(&["0", "apps"])
            .unwrap()
            .as_map()
            .unwrap()
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(apps, vec!["228980", "413150"]);
    }

    #[test]
    fn old_libraryfolders_format_with_escaped_windows_paths() {
        let text = "\"LibraryFolders\"\n{\n\t\"TimeNextStatsReport\"\t\t\"1600000000\"\n\t\"ContentStatsID\"\t\t\"-123\"\n\t\"1\"\t\t\"D:\\\\Games\\\\Steam\"\n\t\"2\"\t\t\"/mnt/other\"\n}\n";
        let root = parse(text).unwrap();
        let lf = root.get("libraryfolders").unwrap();
        assert_eq!(lf.get_str("1"), Some("D:\\Games\\Steam"));
        assert_eq!(lf.get_str("2"), Some("/mnt/other"));
    }

    #[test]
    fn escapes_unquoted_tokens_and_comments() {
        let text = r#"
// leading comment
Root // trailing comment
{
    key value
    "quote" "say \"hi\"\tthere\nnext"
    "lit" "C:\Program Files\x"
    "slashes" "a\\b"
    url "http://x" // comment after url
    nested { inner "1" }
}
"#;
        let root = parse(text).unwrap();
        let r = root.get("root").unwrap();
        assert_eq!(r.get_str("key"), Some("value"));
        assert_eq!(r.get_str("quote"), Some("say \"hi\"\tthere\nnext"));
        assert_eq!(r.get_str("lit"), Some("C:\\Program Files\\x"));
        assert_eq!(r.get_str("slashes"), Some("a\\b"));
        assert_eq!(r.get_str("url"), Some("http://x"));
        assert_eq!(
            r.get_path(&["nested", "inner"]).and_then(VdfNode::as_str),
            Some("1")
        );
    }

    #[test]
    fn conditionals_and_directives_are_ignored() {
        let text = "#base \"other.vdf\"\n#include \"more.vdf\"\n\"A\"\n{\n  \"x\" \"1\" [$WIN32]\n  \"y\" [!$X360] \"2\"\n  \"b\" [$LINUX] { \"z\" \"3\" }\n}\n";
        let (root, warnings) = parse_with_warnings(text).unwrap();
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].contains("line 1") && warnings[0].contains("other.vdf"));
        assert!(warnings[1].contains("line 2"));
        let a = root.get("a").unwrap();
        assert_eq!(a.get_str("x"), Some("1"));
        assert_eq!(a.get_str("y"), Some("2"));
        assert_eq!(a.get_path(&["b", "z"]).and_then(VdfNode::as_str), Some("3"));
    }

    #[test]
    fn appmanifest_parsing() {
        let text = "\"AppState\"\n{\n\t\"appid\"\t\t\"413150\"\n\t\"name\"\t\t\"Stardew Valley\"\n\t\"installdir\"\t\t\"Stardew Valley\"\n\t\"UserConfig\"\n\t{\n\t\t\"language\"\t\t\"english\"\n\t}\n}\n";
        let root = parse(text).unwrap();
        let st = root.get("appstate").unwrap();
        assert_eq!(st.get_str("AppID"), Some("413150"));
        assert_eq!(st.get_str("installdir"), Some("Stardew Valley"));
    }

    fn line_of(e: DiscoveryError) -> usize {
        match e {
            DiscoveryError::Vdf { line, .. } => line,
            other => panic!("unexpected error {other}"),
        }
    }

    #[test]
    fn unterminated_string_reports_start_line() {
        let text = "\"a\"\n{\n  \"b\" \"never closed\n}\n";
        assert_eq!(line_of(parse(text).unwrap_err()), 3);
    }

    #[test]
    fn missing_closing_brace_reports_error() {
        let text = "\"a\"\n{\n  \"b\" \"1\"\n";
        let e = parse(text).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("line 2"), "{msg}");
        assert_eq!(line_of(e), 4);
    }

    #[test]
    fn stray_closing_brace_and_missing_value() {
        assert_eq!(line_of(parse("\"a\" \"b\"\n}\n").unwrap_err()), 2);
        assert_eq!(line_of(parse("\"a\"\n{\n\"k\"\n}\n").unwrap_err()), 4);
        assert_eq!(line_of(parse("\n\n\"a\"").unwrap_err()), 3);
        assert_eq!(line_of(parse("{ }").unwrap_err()), 1);
    }

    #[test]
    fn empty_input_and_bom() {
        assert_eq!(parse("").unwrap(), VdfNode::Map(vec![]));
        let root = parse("\u{feff}\"a\" \"b\"").unwrap();
        assert_eq!(root.get_str("A"), Some("b"));
    }
}
