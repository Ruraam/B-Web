//! JavaScript source tokenizer for the B-Web runtime.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    Identifier,
    Number,
    String,
    Keyword,
    Operator,
    Punctuation,
    Template,
    Eof,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexError {
    pub offset: usize,
    pub message: String,
}

const KEYWORDS: &[&str] = &[
    "var",
    "let",
    "const",
    "function",
    "return",
    "if",
    "else",
    "while",
    "for",
    "break",
    "continue",
    "true",
    "false",
    "null",
    "undefined",
    "typeof",
    "new",
];

/// Maximum number of tokens produced for one script. Script sources are
/// already size-limited on download, but an explicit parse bound prevents a
/// huge inline script from monopolising the UI thread during tokenization.
pub(crate) const MAX_PARSE_TOKENS: usize = 200_000;

fn push_token(tokens: &mut Vec<Token>, token: Token, limit: usize) -> Result<(), LexError> {
    if tokens.len() >= limit {
        return Err(LexError { offset: 0, message: "maximum script token count exceeded".into() });
    }
    tokens.push(token);
    Ok(())
}

/// Tokenizes a JavaScript source string, applying the default per-script limit.
pub fn tokenize(source: &str) -> Result<Vec<Token>, LexError> {
    tokenize_with_limit(source, MAX_PARSE_TOKENS)
}

/// Tokenizes a JavaScript source string while producing at most `limit` tokens
/// (including the final end-of-file token).
pub(crate) fn tokenize_with_limit(source: &str, limit: usize) -> Result<Vec<Token>, LexError> {
    let chars: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        if ch.is_whitespace() {
            index += 1;
            continue;
        }
        if ch == '/' && chars.get(index + 1) == Some(&'/') {
            while index < chars.len() && chars[index] != '\n' {
                index += 1;
            }
            continue;
        }
        if ch == '/' && chars.get(index + 1) == Some(&'*') {
            index += 2;
            while index + 1 < chars.len() && !(chars[index] == '*' && chars[index + 1] == '/') {
                index += 1;
            }
            if index + 1 >= chars.len() {
                return Err(LexError { offset: index, message: "unterminated comment".into() });
            }
            index += 2;
            continue;
        }
        if ch == '`' {
            index += 1;
            let start = index;
            while index < chars.len() && chars[index] != '`' {
                if chars[index] == '$' && chars.get(index + 1) == Some(&'{') {
                    index += 2;
                    let mut depth = 1usize;
                    while index < chars.len() && depth > 0 {
                        if chars[index] == '{' {
                            depth += 1;
                        } else if chars[index] == '}' {
                            depth -= 1;
                        } else if chars[index] == '`' {
                            return Err(LexError {
                                offset: index,
                                message: "unterminated template".into(),
                            });
                        }
                        index += 1;
                    }
                } else {
                    index += 1;
                }
            }
            if index >= chars.len() {
                return Err(LexError { offset: index, message: "unterminated template".into() });
            }
            push_token(
                &mut tokens,
                Token { kind: TokenKind::Template, text: chars[start..index].iter().collect() },
                limit,
            )?;
            index += 1;
            continue;
        }
        if ch == '"' || ch == '\'' {
            let quote = ch;
            index += 1;
            let mut text = String::new();
            while index < chars.len() && chars[index] != quote {
                if chars[index] == '\\' && index + 1 < chars.len() {
                    index += 1;
                    text.push(match chars[index] {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        other => other,
                    });
                } else if chars[index] == '\n' {
                    return Err(LexError { offset: index, message: "unterminated string".into() });
                } else {
                    text.push(chars[index]);
                }
                index += 1;
            }
            if index >= chars.len() {
                return Err(LexError { offset: index, message: "unterminated string".into() });
            }
            index += 1;
            push_token(&mut tokens, Token { kind: TokenKind::String, text }, limit)?;
            continue;
        }
        if ch.is_ascii_digit()
            || (ch == '.' && chars.get(index + 1).is_some_and(|c| c.is_ascii_digit()))
        {
            let start = index;
            while index < chars.len() && chars[index].is_ascii_digit() {
                index += 1;
            }
            if chars.get(index) == Some(&'.') {
                index += 1;
                while index < chars.len() && chars[index].is_ascii_digit() {
                    index += 1;
                }
            }
            push_token(
                &mut tokens,
                Token { kind: TokenKind::Number, text: chars[start..index].iter().collect() },
                limit,
            )?;
            continue;
        }
        if is_identifier_start(ch) {
            let start = index;
            while index < chars.len() && is_identifier_part(chars[index]) {
                index += 1;
            }
            let text: String = chars[start..index].iter().collect();
            let kind = if KEYWORDS.contains(&text.as_str()) {
                TokenKind::Keyword
            } else {
                TokenKind::Identifier
            };
            push_token(&mut tokens, Token { kind, text }, limit)?;
            continue;
        }
        let two =
            if index + 1 < chars.len() { Some([chars[index], chars[index + 1]]) } else { None };
        if matches!(
            two,
            Some(['=', '='])
                | Some(['!', '='])
                | Some(['<', '='])
                | Some(['>', '='])
                | Some(['+', '+'])
                | Some(['-', '-'])
                | Some(['+', '='])
                | Some(['-', '='])
                | Some(['*', '='])
                | Some(['/', '='])
                | Some(['&', '&'])
                | Some(['|', '|'])
                | Some(['=', '>'])
        ) {
            let text: String = two.expect("two-char operator").iter().collect();
            index += 2;
            push_token(&mut tokens, Token { kind: TokenKind::Operator, text }, limit)?;
            continue;
        }
        if matches!(ch, '+' | '-' | '*' | '/' | '%' | '=' | '<' | '>' | '!' | '&' | '|') {
            push_token(
                &mut tokens,
                Token { kind: TokenKind::Operator, text: ch.to_string() },
                limit,
            )?;
        } else if matches!(ch, '(' | ')' | '{' | '}' | '[' | ']' | ';' | ',' | '.' | ':') {
            push_token(
                &mut tokens,
                Token { kind: TokenKind::Punctuation, text: ch.to_string() },
                limit,
            )?;
        } else {
            return Err(LexError {
                offset: index,
                message: format!("unexpected character '{ch}'"),
            });
        }
        index += 1;
    }
    push_token(&mut tokens, Token { kind: TokenKind::Eof, text: String::new() }, limit)?;
    Ok(tokens)
}

fn is_identifier_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_' || ch == '$'
}

fn is_identifier_part(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '$'
}

#[cfg(test)]
mod tests {
    use super::{TokenKind, tokenize};

    #[test]
    fn tokenizes_keywords_operators_and_strings() {
        let tokens = tokenize("let x = 1 + 2; // note\nx == \"hi\";").expect("tokens");
        assert_eq!(tokens[0].kind, TokenKind::Keyword);
        assert_eq!(tokens[0].text, "let");
        assert!(tokens.iter().any(|token| token.text == "=="));
        assert!(tokens.iter().any(|token| token.kind == TokenKind::String && token.text == "hi"));
    }
}
