//! Initial CSS lexer, rule parser, selector matcher, cascade, and computed styles.
//!
//! This is an intentionally documented CSS subset, not a complete CSS Syntax or
//! Selectors implementation. Unsupported at-rules are skipped safely.

use bweb_html::{HtmlDocument, NodeId, NodeKind};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashMap, HashSet},
    error::Error,
    fmt,
};

/// Maximum nesting depth for recursive CSS parsing (nested `@media`/`@layer`
/// blocks and nested `:not()` selectors). Resource size limits alone do not
/// bound recursion depth, so this guard prevents stack exhaustion on hostile
/// stylesheets.
const MAX_PARSE_DEPTH: usize = 64;
/// Maximum number of compounds in a single selector. Selector matching walks
/// compounds recursively, so an unbounded chain of combinators is as dangerous
/// as unbounded `:not()` nesting.
const MAX_SELECTOR_COMPOUNDS: usize = 64;
/// Work units allowed for selector matching during one style computation.
/// This bounds the total cost of descendant/sibling backtracking, which the
/// per-selector compound limit alone cannot cap.
const MATCH_WORK_BUDGET: u64 = 1_000_000;

/// Shared counter limiting selector-matching work for one computation.
#[derive(Debug, Clone)]
struct MatchBudget {
    remaining: u64,
}

impl MatchBudget {
    fn new(limit: u64) -> Self {
        Self { remaining: limit }
    }

    fn exhausted(&self) -> bool {
        self.remaining == 0
    }

    /// Consumes one unit; returns false when no work is left.
    fn tick(&mut self) -> bool {
        if self.remaining == 0 {
            return false;
        }
        self.remaining -= 1;
        true
    }
}

/// A token emitted by the CSS lexer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub text: String,
    pub offset: usize,
}

/// CSS token category used by the initial parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Ident,
    AtKeyword,
    Hash,
    String,
    Number,
    Whitespace,
    Colon,
    Semicolon,
    Comma,
    OpenBrace,
    CloseBrace,
    OpenBracket,
    CloseBracket,
    OpenParen,
    CloseParen,
    Delim,
}

/// A lexer or parser error with its source byte offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CssError {
    pub offset: usize,
    pub message: String,
}

impl CssError {
    fn new(offset: usize, message: impl Into<String>) -> Self {
        Self { offset, message: message.into() }
    }
}

impl fmt::Display for CssError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CSS error at byte {}: {}", self.offset, self.message)
    }
}

impl Error for CssError {}

/// Lexes a CSS source string into a compact token stream.
pub fn tokenize(source: &str) -> Result<Vec<Token>, CssError> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let start = index;
        let byte = bytes[index];
        if byte.is_ascii_whitespace() {
            index += 1;
            while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            tokens.push(token(TokenKind::Whitespace, source, start, index));
            continue;
        }
        if byte == b'/' && bytes.get(index + 1) == Some(&b'*') {
            index += 2;
            let Some(relative_end) = source[index..].find("*/") else {
                return Err(CssError::new(start, "unterminated comment"));
            };
            index += relative_end + 2;
            continue;
        }
        if byte == b'"' || byte == b'\'' {
            let quote = byte;
            index += 1;
            let mut escaped = false;
            while index < bytes.len() {
                let current = bytes[index];
                index += 1;
                if escaped {
                    escaped = false;
                } else if current == b'\\' {
                    escaped = true;
                } else if current == quote {
                    break;
                }
            }
            if bytes.get(index.wrapping_sub(1)) != Some(&quote) {
                return Err(CssError::new(start, "unterminated string"));
            }
            tokens.push(token(TokenKind::String, source, start, index));
            continue;
        }
        if byte == b'@' {
            index += 1;
            while index < bytes.len() && is_name_byte(bytes[index]) {
                index += 1;
            }
            tokens.push(token(TokenKind::AtKeyword, source, start, index));
            continue;
        }
        if byte == b'#' {
            index += 1;
            while index < bytes.len() && is_name_byte(bytes[index]) {
                index += 1;
            }
            tokens.push(token(TokenKind::Hash, source, start, index));
            continue;
        }
        if byte.is_ascii_digit()
            || (byte == b'.' && bytes.get(index + 1).is_some_and(u8::is_ascii_digit))
        {
            index += 1;
            while index < bytes.len() && (bytes[index].is_ascii_digit() || bytes[index] == b'.') {
                index += 1;
            }
            tokens.push(token(TokenKind::Number, source, start, index));
            continue;
        }
        if is_name_start(byte) || byte == b'-' {
            index += 1;
            while index < bytes.len() && is_name_byte(bytes[index]) {
                index += 1;
            }
            tokens.push(token(TokenKind::Ident, source, start, index));
            continue;
        }
        index += 1;
        let kind = match byte {
            b':' => TokenKind::Colon,
            b';' => TokenKind::Semicolon,
            b',' => TokenKind::Comma,
            b'{' => TokenKind::OpenBrace,
            b'}' => TokenKind::CloseBrace,
            b'[' => TokenKind::OpenBracket,
            b']' => TokenKind::CloseBracket,
            b'(' => TokenKind::OpenParen,
            b')' => TokenKind::CloseParen,
            _ => TokenKind::Delim,
        };
        tokens.push(token(kind, source, start, index));
    }
    Ok(tokens)
}

fn token(kind: TokenKind, source: &str, start: usize, end: usize) -> Token {
    Token { kind, text: source[start..end].to_owned(), offset: start }
}

fn is_name_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte >= 0x80
}

fn is_name_byte(byte: u8) -> bool {
    is_name_start(byte) || byte.is_ascii_digit() || byte == b'-'
}

/// One CSS property/value declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    pub property: String,
    pub value: String,
    pub important: bool,
}

impl Declaration {
    #[must_use]
    pub fn new(property: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            property: property.into().trim().to_ascii_lowercase(),
            value: value.into().trim().to_owned(),
            important: false,
        }
    }

    #[must_use]
    pub fn important(mut self) -> Self {
        self.important = true;
        self
    }
}

/// A parsed style rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyleRule {
    pub selectors: Vec<ComplexSelector>,
    pub declarations: Vec<Declaration>,
    pub source_order: usize,
    /// Raw `@media` prelude, when the rule came from a conditional block.
    pub media: Option<String>,
    /// Layer name, when the rule came from a `@layer` block.
    pub layer: Option<String>,
}

/// Parsed stylesheet containing supported qualified rules.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stylesheet {
    pub rules: Vec<StyleRule>,
}

impl Stylesheet {
    /// Parses supported style rules; unsupported at-rules are skipped.
    pub fn parse(source: &str) -> Result<Self, CssError> {
        parse_tokens(&tokenize(source)?)
    }
}

fn parse_tokens(tokens: &[Token]) -> Result<Stylesheet, CssError> {
    parse_tokens_at_depth(tokens, 0)
}

fn parse_tokens_at_depth(tokens: &[Token], depth: usize) -> Result<Stylesheet, CssError> {
    let mut rules = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        skip_whitespace(tokens, &mut index);
        if index >= tokens.len() {
            break;
        }
        if tokens[index].kind == TokenKind::AtKeyword {
            let name = tokens[index].text.trim_start_matches('@').to_ascii_lowercase();
            index += 1;
            let prelude_start = index;
            while index < tokens.len()
                && tokens[index].kind != TokenKind::Semicolon
                && tokens[index].kind != TokenKind::OpenBrace
            {
                index += 1;
            }
            if index < tokens.len() && tokens[index].kind == TokenKind::Semicolon {
                index += 1;
                continue;
            }
            if index >= tokens.len() {
                break;
            }
            let condition = tokens[prelude_start..index]
                .iter()
                .map(|token| token.text.as_str())
                .collect::<String>();
            let block_start = index + 1;
            let block_end = skip_block(tokens, index)?;
            let nests = name == "media" || (name == "layer" && !condition.trim().is_empty());
            // Check before recursing: the token slice may be large even when
            // the remaining stack is not.
            if nests && depth >= MAX_PARSE_DEPTH {
                return Err(CssError::new(
                    tokens[index].offset,
                    "maximum CSS nesting depth exceeded",
                ));
            }
            if name == "media" {
                let nested = parse_tokens_at_depth(&tokens[block_start..block_end - 1], depth + 1)?;
                for mut rule in nested.rules {
                    rule.media = Some(condition.trim().to_owned());
                    rule.source_order = rules.len();
                    rules.push(rule);
                }
            } else if name == "layer" && !condition.trim().is_empty() {
                let nested = parse_tokens_at_depth(&tokens[block_start..block_end - 1], depth + 1)?;
                let layer_name = condition.trim().to_owned();
                for mut rule in nested.rules {
                    rule.layer = Some(layer_name.clone());
                    rule.source_order = rules.len();
                    rules.push(rule);
                }
            }
            index = block_end;
            continue;
        }

        let prelude_start = index;
        while index < tokens.len() && tokens[index].kind != TokenKind::OpenBrace {
            if tokens[index].kind == TokenKind::CloseBrace {
                return Err(CssError::new(tokens[index].offset, "unexpected closing brace"));
            }
            index += 1;
        }
        if index == tokens.len() {
            if prelude_start != index {
                return Err(CssError::new(
                    tokens[prelude_start].offset,
                    "rule has no declaration block",
                ));
            }
            break;
        }
        let selector_text = tokens[prelude_start..index]
            .iter()
            .map(|token| token.text.as_str())
            .collect::<String>();
        let open_offset = tokens[index].offset;
        index += 1;
        let block_start = index;
        let mut depth = 1usize;
        while index < tokens.len() && depth > 0 {
            match tokens[index].kind {
                TokenKind::OpenBrace => depth += 1,
                TokenKind::CloseBrace => depth -= 1,
                _ => {}
            }
            if depth > 0 {
                index += 1;
            }
        }
        if depth != 0 {
            return Err(CssError::new(open_offset, "unterminated declaration block"));
        }
        let declarations = parse_declarations(&tokens[block_start..index]);
        let selectors = parse_selector_list(&selector_text)?;
        if !selectors.is_empty() && !declarations.is_empty() {
            rules.push(StyleRule {
                selectors,
                declarations,
                source_order: rules.len(),
                media: None,
                layer: None,
            });
        }
        index += 1;
    }
    Ok(Stylesheet { rules })
}

fn skip_whitespace(tokens: &[Token], index: &mut usize) {
    while *index < tokens.len() && tokens[*index].kind == TokenKind::Whitespace {
        *index += 1;
    }
}

fn skip_block(tokens: &[Token], open: usize) -> Result<usize, CssError> {
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate().skip(open) {
        match token.kind {
            TokenKind::OpenBrace => depth += 1,
            TokenKind::CloseBrace => {
                depth -= 1;
                if depth == 0 {
                    return Ok(index + 1);
                }
            }
            _ => {}
        }
    }
    Err(CssError::new(tokens[open].offset, "unterminated at-rule block"))
}

/// Evaluates a `@media` prelude against a viewport in CSS pixels.
fn media_matches(condition: &str, width: f32, height: f32) -> bool {
    let condition = condition.trim();
    if condition.is_empty() {
        return true;
    }
    split_top_level(condition, ',')
        .iter()
        .any(|query| media_query_matches(query.trim(), width, height))
}

fn media_query_matches(query: &str, width: f32, height: f32) -> bool {
    let lower = query.to_ascii_lowercase();
    let mut text = lower.trim();
    if let Some(rest) = text.strip_prefix("only ") {
        text = rest.trim();
    }
    if let Some(rest) = text.strip_prefix("screen and ") {
        text = rest.trim();
    } else if let Some(rest) = text.strip_prefix("all and ") {
        text = rest.trim();
    } else if text == "screen" || text == "all" {
        return true;
    } else if text == "print" {
        return false;
    }
    text.split(" and ").all(|feature| media_feature_matches(feature.trim(), width, height))
}

fn media_feature_matches(feature: &str, width: f32, height: f32) -> bool {
    let feature = feature.trim().trim_start_matches('(').trim_end_matches(')').trim();
    let Some((name, value)) = feature.split_once(':') else {
        return false;
    };
    let name = name.trim();
    let value = value.trim();
    if name == "orientation" {
        return (value == "portrait" && height >= width)
            || (value == "landscape" && width > height);
    }
    let Some(length) = parse_media_length(value) else {
        return false;
    };
    match name {
        "width" => (width - length).abs() < 0.5,
        "height" => (height - length).abs() < 0.5,
        "min-width" => width >= length,
        "max-width" => width <= length,
        "min-height" => height >= length,
        "max-height" => height <= length,
        _ => false,
    }
}

fn parse_media_length(value: &str) -> Option<f32> {
    let number = value.strip_suffix("px").unwrap_or(value).trim();
    let length = number.parse::<f32>().ok()?;
    (length.is_finite() && length >= 0.0).then_some(length)
}

fn parse_declarations(tokens: &[Token]) -> Vec<Declaration> {
    let mut declarations = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;
    for index in 0..=tokens.len() {
        let at_end = index == tokens.len();
        if !at_end {
            match tokens[index].kind {
                TokenKind::OpenParen | TokenKind::OpenBracket => depth += 1,
                TokenKind::CloseParen | TokenKind::CloseBracket => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        if at_end || (tokens[index].kind == TokenKind::Semicolon && depth == 0) {
            let part = &tokens[start..index];
            if !part.is_empty() {
                if let Some(colon) = part.iter().position(|token| token.kind == TokenKind::Colon) {
                    let property =
                        part[..colon].iter().map(|token| token.text.as_str()).collect::<String>();
                    let property = property.trim().to_ascii_lowercase();
                    let mut value = part[colon + 1..]
                        .iter()
                        .map(|token| token.text.as_str())
                        .collect::<String>();
                    value = value.trim().to_owned();
                    if !property.is_empty() && !value.is_empty() {
                        let important = value.to_ascii_lowercase().ends_with("!important");
                        if important {
                            value.truncate(value.len() - "!important".len());
                            value = value.trim_end().to_owned();
                        }
                        declarations.push(Declaration { property, value, important });
                    }
                }
            }
            start = index + 1;
        }
    }
    declarations
}

/// Specificity tuple (IDs, classes/attributes/pseudo-classes, type selectors).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Specificity(pub u32, pub u32, pub u32);

/// A generated-content pseudo-element targeted by a selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PseudoElement {
    Before,
    After,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComplexSelector {
    compounds: Vec<CompoundSelector>,
    combinators: Vec<Combinator>,
    pub specificity: Specificity,
    pub pseudo_element: Option<PseudoElement>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Combinator {
    Descendant,
    Child,
    AdjacentSibling,
    GeneralSibling,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CompoundSelector {
    tag: Option<String>,
    universal: bool,
    id: Option<String>,
    classes: Vec<String>,
    attributes: Vec<AttributeSelector>,
    pseudos: Vec<PseudoClass>,
    pseudo_element: Option<PseudoElement>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttributeOperator {
    Exists,
    Equals,
    Includes,
    DashMatch,
    Prefix,
    Suffix,
    Substring,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AttributeSelector {
    name: String,
    operator: AttributeOperator,
    value: Option<String>,
    case_insensitive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Nth {
    Odd,
    Even,
    Formula { a: i32, b: i32 },
}

impl Nth {
    fn parse(source: &str) -> Option<Self> {
        let value = source.trim().to_ascii_lowercase().replace(' ', "");
        match value.as_str() {
            "odd" => return Some(Self::Odd),
            "even" => return Some(Self::Even),
            _ => {}
        }
        let (a, b) = match value.split_once('n') {
            Some(("", rest)) => (1, parse_signed(rest)?),
            Some((coefficient, rest)) => {
                let coefficient = match coefficient {
                    "+" => 1,
                    "-" => -1,
                    _ => coefficient.parse::<i32>().ok()?,
                };
                (coefficient, parse_signed(rest)?)
            }
            None => (0, value.parse::<i32>().ok()?),
        };
        Some(Self::Formula { a, b })
    }

    fn matches(&self, index: usize) -> bool {
        let index = index as i32;
        match *self {
            Self::Odd => index % 2 == 1,
            Self::Even => index % 2 == 0,
            Self::Formula { a: 0, b } => index == b,
            Self::Formula { a, b } => {
                let difference = index - b;
                difference >= 0 && a != 0 && difference % a == 0
            }
        }
    }
}

fn parse_signed(value: &str) -> Option<i32> {
    if value.is_empty() || value == "+" {
        return Some(0);
    }
    value.parse::<i32>().ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PseudoClass {
    Root,
    FirstChild,
    LastChild,
    OnlyChild,
    FirstOfType,
    LastOfType,
    Empty,
    AnyLink,
    NthChild(Nth),
    NthLastChild(Nth),
    Not(Box<CompoundSelector>),
}

fn parse_selector_list(source: &str) -> Result<Vec<ComplexSelector>, CssError> {
    let mut selectors = Vec::new();
    for part in split_top_level(source, ',') {
        let selector = part.trim();
        if !selector.is_empty() {
            selectors.push(parse_complex_selector(selector)?);
        }
    }
    Ok(selectors)
}

fn split_top_level(source: &str, separator: char) -> Vec<&str> {
    let mut result = Vec::new();
    let mut start = 0;
    let mut bracket = 0usize;
    let mut paren = 0usize;
    let mut quote = None;
    for (index, character) in source.char_indices() {
        if let Some(current) = quote {
            if character == current {
                quote = None;
            }
            continue;
        }
        match character {
            '"' | '\'' => quote = Some(character),
            '[' => bracket += 1,
            ']' => bracket = bracket.saturating_sub(1),
            '(' => paren += 1,
            ')' => paren = paren.saturating_sub(1),
            value if value == separator && bracket == 0 && paren == 0 => {
                result.push(&source[start..index]);
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    result.push(&source[start..]);
    result
}

fn parse_complex_selector(source: &str) -> Result<ComplexSelector, CssError> {
    let mut compounds = Vec::new();
    let mut combinators = Vec::new();
    let chars: Vec<char> = source.chars().collect();
    let mut index = 0;
    let mut specificity = Specificity::default();
    while index < chars.len() {
        // Reject long chains before parsing another compound: the matcher
        // recurses once per compound and must not be given an unbounded chain.
        if compounds.len() >= MAX_SELECTOR_COMPOUNDS {
            return Err(CssError::new(0, "maximum selector length exceeded"));
        }
        let mut consumed_space = false;
        while index < chars.len() && chars[index].is_ascii_whitespace() {
            index += 1;
            consumed_space = true;
        }
        if index >= chars.len() {
            break;
        }
        let combinator = match chars[index] {
            '>' => Some(Combinator::Child),
            '+' => Some(Combinator::AdjacentSibling),
            '~' => Some(Combinator::GeneralSibling),
            _ => None,
        };
        if let Some(combinator) = combinator {
            if compounds.is_empty() || combinators.len() + 1 != compounds.len() {
                return Err(CssError::new(0, "misplaced combinator"));
            }
            index += 1;
            while index < chars.len() && chars[index].is_ascii_whitespace() {
                index += 1;
            }
            if index >= chars.len() {
                return Err(CssError::new(0, "selector cannot end with a combinator"));
            }
            let compound = parse_compound(&chars, &mut index, &mut specificity, 0)?;
            combinators.push(combinator);
            compounds.push(compound);
            continue;
        }
        if !compounds.is_empty() {
            if consumed_space {
                combinators.push(Combinator::Descendant);
            } else {
                return Err(CssError::new(0, "expected whitespace or child combinator"));
            }
        }
        let compound = parse_compound(&chars, &mut index, &mut specificity, 0)?;
        compounds.push(compound);
    }
    if compounds.is_empty() || combinators.len() + 1 != compounds.len() {
        return Err(CssError::new(0, "invalid selector"));
    }
    let pseudo_element = compounds.last().and_then(|compound| compound.pseudo_element);
    if let Some(last) = compounds.last_mut() {
        last.pseudo_element = None;
    }
    Ok(ComplexSelector { compounds, combinators, specificity, pseudo_element })
}

fn parse_compound(
    chars: &[char],
    index: &mut usize,
    specificity: &mut Specificity,
    depth: usize,
) -> Result<CompoundSelector, CssError> {
    let mut compound = CompoundSelector::default();
    let mut consumed = false;
    if *index < chars.len() && (chars[*index] == '*' || is_ident_start(chars[*index])) {
        if chars[*index] == '*' {
            compound.universal = true;
            *index += 1;
        } else {
            compound.tag = Some(read_ident(chars, index).to_ascii_lowercase());
            specificity.2 += 1;
        }
        consumed = true;
    }
    while *index < chars.len() {
        match chars[*index] {
            '#' => {
                *index += 1;
                let value = read_ident(chars, index);
                if value.is_empty() {
                    return Err(CssError::new(0, "empty ID selector"));
                }
                compound.id = Some(value);
                specificity.0 += 1;
                consumed = true;
            }
            '.' => {
                *index += 1;
                let value = read_ident(chars, index);
                if value.is_empty() {
                    return Err(CssError::new(0, "empty class selector"));
                }
                compound.classes.push(value);
                specificity.1 += 1;
                consumed = true;
            }
            '[' => {
                *index += 1;
                skip_char_space(chars, index);
                let name = read_ident(chars, index).to_ascii_lowercase();
                if name.is_empty() {
                    return Err(CssError::new(0, "empty attribute selector"));
                }
                skip_char_space(chars, index);
                let two_chars = if *index + 1 < chars.len() {
                    Some([chars[*index], chars[*index + 1]])
                } else {
                    None
                };
                let (operator, has_value) = match two_chars {
                    Some(['~', '=']) => (AttributeOperator::Includes, true),
                    Some(['|', '=']) => (AttributeOperator::DashMatch, true),
                    Some(['^', '=']) => (AttributeOperator::Prefix, true),
                    Some(['$', '=']) => (AttributeOperator::Suffix, true),
                    Some(['*', '=']) => (AttributeOperator::Substring, true),
                    Some(['=', _]) => (AttributeOperator::Equals, true),
                    _ => (AttributeOperator::Exists, false),
                };
                if has_value {
                    *index += if matches!(operator, AttributeOperator::Equals) { 1 } else { 2 };
                    skip_char_space(chars, index);
                }
                let value = has_value.then(|| {
                    let value = read_attribute_value(chars, index);
                    skip_char_space(chars, index);
                    value
                });
                let case_insensitive = if *index < chars.len()
                    && matches!(chars[*index], 'i' | 'I')
                    && *index + 1 < chars.len()
                    && chars[*index + 1] == ']'
                {
                    *index += 1;
                    true
                } else {
                    false
                };
                if *index >= chars.len() || chars[*index] != ']' {
                    return Err(CssError::new(0, "unterminated attribute selector"));
                }
                *index += 1;
                compound.attributes.push(AttributeSelector {
                    name,
                    operator,
                    value,
                    case_insensitive,
                });
                specificity.1 += 1;
                consumed = true;
            }
            ':' => {
                *index += 1;
                if *index < chars.len() && chars[*index] == ':' {
                    *index += 1;
                }
                let name = read_ident(chars, index).to_ascii_lowercase();
                if name == "before" || name == "after" {
                    if compound.pseudo_element.is_some() {
                        return Err(CssError::new(0, "duplicate pseudo-element"));
                    }
                    compound.pseudo_element = Some(if name == "before" {
                        PseudoElement::Before
                    } else {
                        PseudoElement::After
                    });
                    specificity.2 += 1;
                    consumed = true;
                    continue;
                }
                let argument = if *index < chars.len() && chars[*index] == '(' {
                    *index += 1;
                    let start = *index;
                    let mut depth = 1usize;
                    while *index < chars.len() && depth > 0 {
                        match chars[*index] {
                            '(' => depth += 1,
                            ')' => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                        *index += 1;
                    }
                    if depth != 0 {
                        return Err(CssError::new(0, "unterminated pseudo-class argument"));
                    }
                    let value = chars[start..*index].iter().collect::<String>();
                    *index += 1;
                    Some(value)
                } else {
                    None
                };
                let pseudo = match name.as_str() {
                    "root" => PseudoClass::Root,
                    "first-child" => PseudoClass::FirstChild,
                    "last-child" => PseudoClass::LastChild,
                    "only-child" => PseudoClass::OnlyChild,
                    "first-of-type" => PseudoClass::FirstOfType,
                    "last-of-type" => PseudoClass::LastOfType,
                    "empty" => PseudoClass::Empty,
                    "link" | "any-link" => PseudoClass::AnyLink,
                    "nth-child" => PseudoClass::NthChild(
                        Nth::parse(argument.as_deref().unwrap_or(""))
                            .ok_or_else(|| CssError::new(0, "invalid :nth-child argument"))?,
                    ),
                    "nth-last-child" => PseudoClass::NthLastChild(
                        Nth::parse(argument.as_deref().unwrap_or(""))
                            .ok_or_else(|| CssError::new(0, "invalid :nth-last-child argument"))?,
                    ),
                    "not" => {
                        if depth >= MAX_PARSE_DEPTH {
                            return Err(CssError::new(0, "maximum CSS nesting depth exceeded"));
                        }
                        let source = argument
                            .ok_or_else(|| CssError::new(0, ":not requires a selector argument"))?;
                        let inner_chars: Vec<char> = source.chars().collect();
                        let mut inner_index = 0;
                        let mut inner_specificity = Specificity::default();
                        let inner = parse_compound(
                            &inner_chars,
                            &mut inner_index,
                            &mut inner_specificity,
                            depth + 1,
                        )?;
                        if inner_index != inner_chars.len() {
                            return Err(CssError::new(0, "unsupported :not argument"));
                        }
                        specificity.0 += inner_specificity.0;
                        specificity.1 += inner_specificity.1;
                        specificity.2 += inner_specificity.2;
                        PseudoClass::Not(Box::new(inner))
                    }
                    _ => return Err(CssError::new(0, format!("unsupported pseudo-class :{name}"))),
                };
                compound.pseudos.push(pseudo);
                if name != "not" {
                    specificity.1 += 1;
                }
                consumed = true;
            }
            _ => break,
        }
    }
    if !consumed {
        return Err(CssError::new(0, "expected a simple selector"));
    }
    Ok(compound)
}

fn is_ident_start(character: char) -> bool {
    character.is_ascii_alphabetic() || character == '_' || character == '-'
}

fn read_ident(chars: &[char], index: &mut usize) -> String {
    let start = *index;
    while *index < chars.len()
        && (chars[*index].is_ascii_alphanumeric() || matches!(chars[*index], '_' | '-'))
    {
        *index += 1;
    }
    chars[start..*index].iter().collect()
}

fn skip_char_space(chars: &[char], index: &mut usize) {
    while *index < chars.len() && chars[*index].is_ascii_whitespace() {
        *index += 1;
    }
}

fn read_attribute_value(chars: &[char], index: &mut usize) -> String {
    if *index < chars.len() && matches!(chars[*index], '"' | '\'') {
        let quote = chars[*index];
        *index += 1;
        let start = *index;
        while *index < chars.len() && chars[*index] != quote {
            *index += 1;
        }
        let value = chars[start..*index].iter().collect();
        if *index < chars.len() {
            *index += 1;
        }
        value
    } else {
        read_ident(chars, index)
    }
}

/// Computed property values for a DOM node.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComputedStyle {
    properties: BTreeMap<String, String>,
}

impl ComputedStyle {
    #[must_use]
    pub fn get(&self, property: &str) -> Option<&str> {
        self.properties.get(&property.to_ascii_lowercase()).map(String::as_str)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.properties.iter().map(|(name, value)| (name.as_str(), value.as_str()))
    }

    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = (&String, &mut String)> {
        self.properties.iter_mut()
    }

    fn set(&mut self, property: String, value: String) {
        self.properties.insert(property, value);
    }

    fn initial() -> Self {
        let mut style = Self::default();
        style.set("display".into(), "inline".into());
        style.set("border-radius".into(), "0".into());
        style.set("position".into(), "static".into());
        style.set("top".into(), "auto".into());
        style.set("right".into(), "auto".into());
        style.set("bottom".into(), "auto".into());
        style.set("left".into(), "auto".into());
        style.set("color".into(), "black".into());
        style.set("background-color".into(), "transparent".into());
        style.set("background-image".into(), "none".into());
        style.set("background-repeat".into(), "repeat".into());
        style.set("background-position".into(), "0% 0%".into());
        style.set("background-size".into(), "auto".into());
        style.set("background-attachment".into(), "scroll".into());
        style.set("background-origin".into(), "padding-box".into());
        style.set("background-clip".into(), "border-box".into());
        style.set("font-size".into(), "16px".into());
        style.set("font-family".into(), "serif".into());
        style.set("font-weight".into(), "normal".into());
        style.set("font-style".into(), "normal".into());
        style.set("line-height".into(), "normal".into());
        style.set("visibility".into(), "visible".into());
        style.set("white-space".into(), "normal".into());
        style.set("flex-direction".into(), "row".into());
        style.set("flex-wrap".into(), "nowrap".into());
        style.set("justify-content".into(), "flex-start".into());
        style.set("align-items".into(), "stretch".into());
        style.set("flex-grow".into(), "0".into());
        style.set("flex-shrink".into(), "1".into());
        style.set("flex-basis".into(), "auto".into());
        style.set("align-self".into(), "auto".into());
        style.set("row-gap".into(), "0".into());
        style.set("column-gap".into(), "0".into());
        style.set("grid-template-columns".into(), "none".into());
        style.set("grid-template-rows".into(), "none".into());
        style.set("grid-auto-rows".into(), "auto".into());
        style.set("grid-column".into(), "auto".into());
        style.set("grid-row".into(), "auto".into());
        style.set("grid-template-areas".into(), "none".into());
        style.set("grid-area".into(), "auto".into());
        style.set("opacity".into(), "1".into());
        style.set("filter".into(), "none".into());
        style.set("box-shadow".into(), "none".into());
        style.set("transform-origin".into(), "50% 50%".into());
        style.set("float".into(), "none".into());
        style.set("clear".into(), "none".into());
        style
    }

    fn inherited_from(parent: &Self) -> Self {
        const INHERITED: &[&str] = &[
            "color",
            "font-family",
            "font-size",
            "font-style",
            "font-weight",
            "line-height",
            "visibility",
            "white-space",
            "text-align",
            "text-indent",
            "text-decoration",
            "text-shadow",
            "letter-spacing",
            "word-spacing",
            "direction",
            "cursor",
        ];
        let mut style = Self::initial();
        for property in INHERITED {
            if let Some(value) = parent.properties.get(*property) {
                style.set((*property).to_owned(), value.clone());
            }
        }
        for (property, value) in &parent.properties {
            if property.starts_with("--") {
                style.set(property.clone(), value.clone());
            }
        }
        style
    }
}

/// NodeId-indexed collection of computed styles.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComputedStyleMap {
    styles: HashMap<NodeId, ComputedStyle>,
}

impl ComputedStyleMap {
    #[must_use]
    pub fn get(&self, id: NodeId) -> Option<&ComputedStyle> {
        self.styles.get(&id)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.styles.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.styles.is_empty()
    }

    /// Resolves relative `url(...)` values in computed styles against a base URL.
    /// This lets the paint adapter match them to fetched background images.
    pub fn resolve_urls(&mut self, base: &str) {
        let Ok(base_url) = url::Url::parse(base) else {
            return;
        };
        for style in self.styles.values_mut() {
            for (property, value) in style.iter_mut() {
                if !property.starts_with("background-image") {
                    continue;
                }
                let mut resolved = value.clone();
                let mut cursor = 0;
                while let Some(offset) = resolved[cursor..].find("url(") {
                    let start = cursor + offset + 4;
                    let Some(relative_end) = resolved[start..].find(')') else {
                        break;
                    };
                    let end = start + relative_end;
                    let raw = resolved[start..end].trim().trim_matches(['\'', '"']).trim();
                    if !raw.is_empty() {
                        if let Ok(absolute) = base_url.join(raw) {
                            resolved.replace_range(start..end, absolute.as_str());
                            cursor = start + absolute.as_str().len();
                            continue;
                        }
                    }
                    cursor = end + 1;
                }
                *value = resolved;
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Origin {
    UserAgent,
    Author,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Priority {
    important: bool,
    origin: Origin,
    /// Lower values are earlier layers. `usize::MAX` means unlayered, which
    /// wins over layers for normal declarations and loses for `!important`.
    layer: usize,
    specificity: Specificity,
    order: usize,
}

impl Ord for Priority {
    fn cmp(&self, other: &Self) -> Ordering {
        // Importance has top priority. For important declarations the origin
        // and layer orders are reversed.
        self.important
            .cmp(&other.important)
            .then_with(|| {
                if self.important {
                    other.origin.cmp(&self.origin)
                } else {
                    self.origin.cmp(&other.origin)
                }
            })
            .then_with(|| {
                if self.important {
                    other.layer.cmp(&self.layer)
                } else {
                    self.layer.cmp(&other.layer)
                }
            })
            .then_with(|| self.specificity.cmp(&other.specificity))
            .then_with(|| self.order.cmp(&other.order))
    }
}

impl PartialOrd for Priority {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Applies author stylesheets and inline declarations to an arena DOM.
#[derive(Debug, Clone, Default)]
pub struct StyleEngine;

impl StyleEngine {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Computes styles for every DOM NodeId with a default desktop viewport.
    #[must_use]
    pub fn compute(&self, document: &HtmlDocument, stylesheets: &[Stylesheet]) -> ComputedStyleMap {
        self.compute_with_viewport(document, stylesheets, 1280.0, 720.0)
    }

    /// Computes styles, evaluating `@media` conditions against a viewport in
    /// CSS pixels. Text/comment nodes inherit their parent's inherited
    /// properties; layout-dependent values are not resolved.
    #[must_use]
    pub fn compute_with_viewport(
        &self,
        document: &HtmlDocument,
        stylesheets: &[Stylesheet],
        viewport_width: f32,
        viewport_height: f32,
    ) -> ComputedStyleMap {
        self.compute_with_match_budget(
            document,
            stylesheets,
            viewport_width,
            viewport_height,
            MATCH_WORK_BUDGET,
        )
    }

    /// Computes styles with an explicit selector-matching work budget.
    /// Exhausting the budget stops further matching; declarations already
    /// applied remain, so the caller always receives a usable style map.
    #[must_use]
    pub fn compute_with_match_budget(
        &self,
        document: &HtmlDocument,
        stylesheets: &[Stylesheet],
        viewport_width: f32,
        viewport_height: f32,
        match_budget: u64,
    ) -> ComputedStyleMap {
        let mut budget = MatchBudget::new(match_budget);
        self.compute_with_viewport_inner(
            document,
            stylesheets,
            viewport_width,
            viewport_height,
            &mut budget,
        )
    }

    fn compute_with_viewport_inner(
        &self,
        document: &HtmlDocument,
        stylesheets: &[Stylesheet],
        viewport_width: f32,
        viewport_height: f32,
        budget: &mut MatchBudget,
    ) -> ComputedStyleMap {
        let mut layer_order: HashMap<String, usize> = HashMap::new();
        for sheet in stylesheets {
            for rule in &sheet.rules {
                if let Some(layer) = &rule.layer {
                    if !layer_order.contains_key(layer) {
                        let next = layer_order.len();
                        layer_order.insert(layer.clone(), next);
                    }
                }
            }
        }
        let unlayered = usize::MAX;
        let mut result = ComputedStyleMap::default();
        let mut priorities: HashMap<NodeId, HashMap<String, Priority>> = HashMap::new();
        for (id, node) in document.iter() {
            if document.is_in_template(id) {
                continue;
            }
            let mut computed = node
                .parent()
                .and_then(|parent| result.styles.get(&parent))
                .map(ComputedStyle::inherited_from)
                .unwrap_or_else(ComputedStyle::initial);
            if let NodeKind::Element { tag_name, attributes } = node.kind() {
                // UA stylesheet defaults. Author declarations below can still
                // override them, just like a real user-agent origin sheet.
                if matches!(tag_name.as_str(), "html" | "body") {
                    apply_declaration(
                        &mut computed,
                        priorities.entry(id).or_default(),
                        &Declaration::new("display", "block"),
                        Priority {
                            important: false,
                            origin: Origin::UserAgent,
                            layer: unlayered,
                            specificity: Specificity(0, 0, 0),
                            order: 0,
                        },
                    );
                }
                for (sheet_index, sheet) in stylesheets.iter().enumerate() {
                    if budget.exhausted() {
                        break;
                    }
                    for rule in &sheet.rules {
                        if budget.exhausted() {
                            break;
                        }
                        if rule.media.as_ref().is_some_and(|media| {
                            !media_matches(media, viewport_width, viewport_height)
                        }) {
                            continue;
                        }
                        let best = rule
                            .selectors
                            .iter()
                            .filter(|selector| {
                                !budget.exhausted()
                                    && selector.pseudo_element.is_none()
                                    && matches_selector(document, id, selector, budget)
                            })
                            .map(|selector| selector.specificity)
                            .max();
                        if let Some(specificity) = best {
                            for declaration in &rule.declarations {
                                apply_declaration(
                                    &mut computed,
                                    priorities.entry(id).or_default(),
                                    declaration,
                                    Priority {
                                        important: declaration.important,
                                        origin: Origin::Author,
                                        layer: rule
                                            .layer
                                            .as_ref()
                                            .and_then(|name| layer_order.get(name).copied())
                                            .unwrap_or(unlayered),
                                        specificity,
                                        order: sheet_index.saturating_mul(1_000_000)
                                            + rule.source_order,
                                    },
                                );
                            }
                        }
                    }
                }
                if let Some(style) = attributes.iter().find(|attribute| attribute.name == "style") {
                    if let Ok(inline) = parse_inline_declarations(&style.value) {
                        for (order, declaration) in inline.iter().enumerate() {
                            apply_declaration(
                                &mut computed,
                                priorities.entry(id).or_default(),
                                declaration,
                                Priority {
                                    important: declaration.important,
                                    origin: Origin::Author,
                                    layer: unlayered,
                                    specificity: Specificity(u32::MAX, 0, 0),
                                    order,
                                },
                            );
                        }
                    }
                }
            }
            result.styles.insert(id, computed);
        }
        finalize_styles(&mut result, document, &priorities);
        result
    }

    /// Computes ordinary styles plus the styles of `::before`/`::after` rules.
    ///
    /// Generated styles inherit from their originating element and are
    /// finalized (variables, shorthands) exactly like ordinary ones.
    #[must_use]
    pub fn compute_generated(
        &self,
        document: &HtmlDocument,
        stylesheets: &[Stylesheet],
        viewport_width: f32,
        viewport_height: f32,
    ) -> (ComputedStyleMap, HashMap<(NodeId, PseudoElement), ComputedStyle>) {
        // One budget is shared by the ordinary and generated passes, so an
        // expensive stylesheet cannot double the allowed work.
        let mut budget = MatchBudget::new(MATCH_WORK_BUDGET);
        let base = self.compute_with_viewport_inner(
            document,
            stylesheets,
            viewport_width,
            viewport_height,
            &mut budget,
        );
        let mut layer_order: HashMap<String, usize> = HashMap::new();
        for sheet in stylesheets {
            for rule in &sheet.rules {
                if let Some(layer) = &rule.layer {
                    if !layer_order.contains_key(layer) {
                        let next = layer_order.len();
                        layer_order.insert(layer.clone(), next);
                    }
                }
            }
        }
        let unlayered = usize::MAX;
        let mut generated: HashMap<(NodeId, PseudoElement), ComputedStyle> = HashMap::new();
        let mut priorities: HashMap<(NodeId, PseudoElement), HashMap<String, Priority>> =
            HashMap::new();
        for (id, node) in document.iter() {
            if document.is_in_template(id) || !matches!(node.kind(), NodeKind::Element { .. }) {
                continue;
            }
            for (sheet_index, sheet) in stylesheets.iter().enumerate() {
                if budget.exhausted() {
                    break;
                }
                for rule in &sheet.rules {
                    if budget.exhausted() {
                        break;
                    }
                    if rule
                        .media
                        .as_ref()
                        .is_some_and(|media| !media_matches(media, viewport_width, viewport_height))
                    {
                        continue;
                    }
                    let mut best: HashMap<PseudoElement, Specificity> = HashMap::new();
                    for selector in &rule.selectors {
                        if let Some(pseudo) = selector.pseudo_element {
                            if !budget.exhausted()
                                && matches_selector(document, id, selector, &mut budget)
                            {
                                best.entry(pseudo)
                                    .and_modify(|current| {
                                        *current = (*current).max(selector.specificity)
                                    })
                                    .or_insert(selector.specificity);
                            }
                        }
                    }
                    for (pseudo, specificity) in best {
                        let key = (id, pseudo);
                        let style = generated.entry(key).or_insert_with(|| {
                            base.styles.get(&id).cloned().unwrap_or_else(ComputedStyle::initial)
                        });
                        for declaration in &rule.declarations {
                            apply_declaration(
                                style,
                                priorities.entry(key).or_default(),
                                declaration,
                                Priority {
                                    important: declaration.important,
                                    origin: Origin::Author,
                                    layer: rule
                                        .layer
                                        .as_ref()
                                        .and_then(|name| layer_order.get(name).copied())
                                        .unwrap_or(unlayered),
                                    specificity,
                                    order: sheet_index.saturating_mul(1_000_000)
                                        + rule.source_order,
                                },
                            );
                        }
                    }
                }
            }
        }
        for (key, style) in &mut generated {
            let explicit: HashSet<String> = priorities
                .get(key)
                .map(|properties| properties.keys().cloned().collect())
                .unwrap_or_default();
            finalize_one(style, &explicit);
        }
        (base, generated)
    }
}

fn finalize_styles(
    result: &mut ComputedStyleMap,
    document: &HtmlDocument,
    priorities: &HashMap<NodeId, HashMap<String, Priority>>,
) {
    for (id, _) in document.iter() {
        let Some(style) = result.styles.get_mut(&id) else {
            continue;
        };
        let explicit: HashSet<String> = priorities
            .get(&id)
            .map(|properties| properties.keys().cloned().collect())
            .unwrap_or_default();
        finalize_one(style, &explicit);
    }
}

fn finalize_one(style: &mut ComputedStyle, explicit: &HashSet<String>) {
    let mut variables: BTreeMap<String, String> = BTreeMap::new();
    for (name, value) in style.iter() {
        if name.starts_with("--") {
            let resolved = resolve_value(value, &variables, 0);
            variables.insert(name.to_owned(), resolved);
        }
    }
    if !variables.is_empty() {
        let replacements: Vec<(String, String)> = style
            .iter()
            .filter(|(name, _)| !name.starts_with("--"))
            .map(|(name, value)| (name.to_owned(), resolve_value(value, &variables, 0)))
            .collect();
        for (name, value) in replacements {
            style.set(name, value);
        }
    }
    expand_shorthands(style, explicit);
}

fn set_if_absent(
    style: &mut ComputedStyle,
    explicit: &HashSet<String>,
    property: &str,
    value: &str,
) {
    if !explicit.contains(property) {
        style.set(property.to_owned(), value.to_owned());
    }
}

fn is_length(value: &str) -> bool {
    value.strip_suffix("px").unwrap_or(value).trim().parse::<f32>().is_ok()
}

fn is_color_token(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if lower.starts_with('#') {
        return true;
    }
    if ["rgb(", "rgba(", "hsl(", "hsla("].iter().any(|prefix| lower.starts_with(prefix)) {
        return lower.ends_with(')');
    }
    const NAMED: &[&str] = &[
        "black",
        "white",
        "red",
        "green",
        "blue",
        "yellow",
        "gray",
        "grey",
        "transparent",
        "orange",
        "purple",
        "teal",
        "navy",
        "silver",
        "aqua",
        "fuchsia",
        "lime",
        "maroon",
        "olive",
        "pink",
        "rebeccapurple",
        "cyan",
        "magenta",
        "gold",
        "brown",
        "coral",
        "crimson",
        "darkblue",
        "darkgray",
        "darkgreen",
        "darkgrey",
        "goldenrod",
        "indigo",
        "ivory",
        "khaki",
        "lavender",
        "mistyrose",
        "orchid",
        "salmon",
        "sienna",
        "tan",
        "tomato",
        "violet",
        "wheat",
    ];
    NAMED.contains(&lower.as_str())
}

/// Splits a value on whitespace outside parentheses, brackets and quotes.
/// This keeps a color function such as `rgb(1, 2, 3)` in a single token.
fn split_whitespace_top_level(source: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut start = 0;
    let mut paren = 0usize;
    let mut bracket = 0usize;
    let mut quote = None;
    for (index, character) in source.char_indices() {
        if let Some(current) = quote {
            if character == current {
                quote = None;
            }
            continue;
        }
        match character {
            '"' | '\'' => quote = Some(character),
            '(' => paren += 1,
            ')' => paren = paren.saturating_sub(1),
            '[' => bracket += 1,
            ']' => bracket = bracket.saturating_sub(1),
            value if value.is_ascii_whitespace() && paren == 0 && bracket == 0 => {
                if start < index {
                    result.push(&source[start..index]);
                }
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    if start < source.len() {
        result.push(&source[start..]);
    }
    result
}

/// Expands the `background` shorthand into its longhands. A color may only
/// appear in the final layer; other longhands are joined per layer so all
/// declared layers survive in the computed style.
fn expand_background(style: &mut ComputedStyle, explicit: &HashSet<String>, value: &str) {
    let layers: Vec<&str> = split_top_level(value, ',')
        .into_iter()
        .map(str::trim)
        .filter(|layer| !layer.is_empty())
        .collect();
    if layers.is_empty() {
        return;
    }

    let mut images = Vec::new();
    let mut repeats = Vec::new();
    let mut positions = Vec::new();
    let mut sizes = Vec::new();
    let mut attachments = Vec::new();
    let mut origins = Vec::new();
    let mut clips = Vec::new();
    let mut color = None;

    for (layer_index, layer) in layers.iter().enumerate() {
        let tokens = split_whitespace_top_level(layer);
        let mut image = "none";
        let mut repeat = "repeat";
        let mut attachment = "scroll";
        let mut position: Vec<&str> = Vec::new();
        let mut size: Vec<&str> = Vec::new();
        let mut boxes: Vec<&str> = Vec::new();
        let mut in_size = false;

        for token in tokens {
            let lower = token.to_ascii_lowercase();
            if lower == "/" {
                in_size = true;
            } else if lower == "none" || lower.starts_with("url(") || lower.contains("gradient(") {
                image = token;
            } else if matches!(
                lower.as_str(),
                "repeat" | "no-repeat" | "repeat-x" | "repeat-y" | "space" | "round"
            ) {
                repeat = token;
            } else if matches!(lower.as_str(), "scroll" | "fixed" | "local") {
                attachment = token;
            } else if matches!(lower.as_str(), "border-box" | "padding-box" | "content-box") {
                boxes.push(token);
            } else if in_size || is_background_size_token(&lower) {
                size.push(token);
            } else if is_background_position_token(&lower) {
                position.push(token);
            } else if layer_index + 1 == layers.len() && is_color_token(token) && color.is_none() {
                color = Some(token);
            }
        }

        images.push(image);
        repeats.push(repeat);
        attachments.push(attachment);
        positions.push(if position.is_empty() { "0% 0%".to_owned() } else { position.join(" ") });
        sizes.push(if size.is_empty() { "auto".to_owned() } else { size.join(" ") });
        if boxes.len() >= 2 {
            origins.push(boxes[0]);
            clips.push(boxes[1]);
        } else if let Some(box_value) = boxes.first() {
            origins.push(box_value);
            clips.push(box_value);
        } else {
            origins.push("padding-box");
            clips.push("border-box");
        }
    }

    set_if_absent(style, explicit, "background-image", &images.join(", "));
    set_if_absent(style, explicit, "background-repeat", &repeats.join(", "));
    set_if_absent(style, explicit, "background-position", &positions.join(", "));
    set_if_absent(style, explicit, "background-size", &sizes.join(", "));
    set_if_absent(style, explicit, "background-attachment", &attachments.join(", "));
    set_if_absent(style, explicit, "background-origin", &origins.join(", "));
    set_if_absent(style, explicit, "background-clip", &clips.join(", "));
    if let Some(color) = color {
        set_if_absent(style, explicit, "background-color", color);
    }
}

fn is_background_position_token(value: &str) -> bool {
    matches!(value, "left" | "center" | "right" | "top" | "bottom")
        || is_length(value)
        || value.ends_with('%')
}

fn is_background_size_token(value: &str) -> bool {
    matches!(value, "auto" | "cover" | "contain") || is_length(value) || value.ends_with('%')
}

fn expand_shorthands(style: &mut ComputedStyle, explicit: &HashSet<String>) {
    expand_edges(style, explicit, "margin");
    expand_edges(style, explicit, "padding");
    if let Some(value) = style.get("background").map(str::to_owned) {
        expand_background(style, explicit, &value);
    }
    if let Some(value) = style.get("border").map(str::to_owned) {
        expand_border(style, explicit, "border", &value);
    }
    for side in ["top", "right", "bottom", "left"] {
        let property = format!("border-{side}");
        if let Some(value) = style.get(&property).map(str::to_owned) {
            expand_border(style, explicit, &property, &value);
        }
    }
    if let Some(value) = style.get("font").map(str::to_owned) {
        expand_font(style, explicit, &value);
    }
    if let Some(value) = style.get("flex").map(str::to_owned) {
        expand_flex(style, explicit, &value);
    }
    if let Some(value) = style.get("gap").map(str::to_owned) {
        expand_gap(style, explicit, &value);
    }
}

fn expand_flex(style: &mut ComputedStyle, explicit: &HashSet<String>, value: &str) {
    let lower = value.trim().to_ascii_lowercase();
    if lower == "none" {
        set_if_absent(style, explicit, "flex-grow", "0");
        set_if_absent(style, explicit, "flex-shrink", "0");
        set_if_absent(style, explicit, "flex-basis", "auto");
        return;
    }
    if lower == "auto" {
        set_if_absent(style, explicit, "flex-grow", "1");
        set_if_absent(style, explicit, "flex-shrink", "1");
        set_if_absent(style, explicit, "flex-basis", "auto");
        return;
    }

    let mut numbers = Vec::new();
    let mut basis = None;
    for token in value.split_ascii_whitespace() {
        if let Ok(number) = token.parse::<f32>() {
            numbers.push(number);
        } else if is_length(token) || token.eq_ignore_ascii_case("auto") || token.ends_with('%') {
            basis = Some(token);
        }
    }
    match numbers.as_slice() {
        [grow] => {
            set_if_absent(style, explicit, "flex-grow", &grow.to_string());
            set_if_absent(style, explicit, "flex-shrink", "1");
            set_if_absent(style, explicit, "flex-basis", "0");
        }
        [grow, shrink] => {
            set_if_absent(style, explicit, "flex-grow", &grow.to_string());
            set_if_absent(style, explicit, "flex-shrink", &shrink.to_string());
        }
        [grow, shrink, third] => {
            set_if_absent(style, explicit, "flex-grow", &grow.to_string());
            set_if_absent(style, explicit, "flex-shrink", &shrink.to_string());
            // A unitless third value is a zero/number flex-basis.
            if basis.is_none() {
                basis = Some(if *third == 0.0 { "0" } else { "auto" });
            }
        }
        _ => {}
    }
    if let Some(basis) = basis {
        set_if_absent(style, explicit, "flex-basis", basis);
    }
}

fn expand_gap(style: &mut ComputedStyle, explicit: &HashSet<String>, value: &str) {
    let parts: Vec<&str> = value.split_ascii_whitespace().collect();
    match parts.as_slice() {
        [all] => {
            set_if_absent(style, explicit, "row-gap", all);
            set_if_absent(style, explicit, "column-gap", all);
        }
        [row, column] => {
            set_if_absent(style, explicit, "row-gap", row);
            set_if_absent(style, explicit, "column-gap", column);
        }
        _ => {}
    }
}

fn expand_edges(style: &mut ComputedStyle, explicit: &HashSet<String>, property: &str) {
    let Some(value) = style.get(property).map(str::to_owned) else {
        return;
    };
    let parts: Vec<&str> = value.split_ascii_whitespace().collect();
    let sides = ["top", "right", "bottom", "left"];
    let values = match parts.as_slice() {
        [all] => [*all, *all, *all, *all],
        [vertical, horizontal] => [*vertical, *horizontal, *vertical, *horizontal],
        [top, horizontal, bottom] => [*top, *horizontal, *bottom, *horizontal],
        [top, right, bottom, left] => [*top, *right, *bottom, *left],
        _ => return,
    };
    for (index, side) in sides.iter().enumerate() {
        set_if_absent(style, explicit, &format!("{property}-{side}"), values[index]);
    }
}

fn expand_border(
    style: &mut ComputedStyle,
    explicit: &HashSet<String>,
    property: &str,
    value: &str,
) {
    let tokens: Vec<&str> = value.split_ascii_whitespace().collect();
    if let Some(width) = tokens.iter().find(|token| is_length(token)) {
        set_if_absent(style, explicit, &format!("{property}-width"), width);
    }
    const STYLES: &[&str] = &[
        "solid", "dashed", "dotted", "double", "none", "hidden", "groove", "ridge", "inset",
        "outset",
    ];
    if let Some(border_style) =
        tokens.iter().find(|token| STYLES.contains(&token.to_ascii_lowercase().as_str()))
    {
        set_if_absent(style, explicit, &format!("{property}-style"), border_style);
    }
    if let Some(color) = tokens.iter().rev().find(|token| is_color_token(token)) {
        set_if_absent(style, explicit, &format!("{property}-color"), color);
    }
}

fn expand_font(style: &mut ComputedStyle, explicit: &HashSet<String>, value: &str) {
    let mut family = Vec::new();
    for token in value.split_ascii_whitespace() {
        if let Some((size, line_height)) = token.split_once('/') {
            if is_length(size) {
                set_if_absent(style, explicit, "font-size", size);
            }
            if !line_height.is_empty() {
                set_if_absent(style, explicit, "line-height", line_height);
            }
        } else if is_length(token) {
            set_if_absent(style, explicit, "font-size", token);
        } else if matches!(token.to_ascii_lowercase().as_str(), "italic" | "oblique") {
            set_if_absent(style, explicit, "font-style", token);
        } else if matches!(token.to_ascii_lowercase().as_str(), "bold" | "bolder" | "lighter") {
            set_if_absent(style, explicit, "font-weight", token);
        } else {
            family.push(token);
        }
    }
    if !family.is_empty() {
        set_if_absent(style, explicit, "font-family", &family.join(" "));
    }
}

fn resolve_value(value: &str, variables: &BTreeMap<String, String>, depth: usize) -> String {
    if depth > 8 {
        return value.to_owned();
    }
    let Some(start) = value.find("var(") else {
        return value.to_owned();
    };
    let mut output = String::with_capacity(value.len());
    output.push_str(&value[..start]);
    let mut rest = &value[start..];
    while let Some(offset) = rest.find("var(") {
        output.push_str(&rest[..offset]);
        rest = &rest[offset..];
        let arguments = &rest[4..];
        let mut nesting = 1usize;
        let mut end = None;
        for (index, character) in arguments.char_indices() {
            match character {
                '(' => nesting += 1,
                ')' => {
                    nesting -= 1;
                    if nesting == 0 {
                        end = Some(index);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = end else {
            output.push_str(rest);
            return output;
        };
        let inner = &arguments[..end];
        let parts = split_top_level(inner, ',');
        let name = parts.first().map_or("", |part| part.trim());
        let fallback = parts.get(1).map(|part| part.trim());
        if let Some(replacement) = variables
            .get(name)
            .cloned()
            .or_else(|| fallback.map(|fallback| resolve_value(fallback, variables, depth + 1)))
        {
            output.push_str(&resolve_value(&replacement, variables, depth + 1));
        } else {
            output.push_str("var(");
            output.push_str(inner);
            output.push(')');
        }
        rest = &arguments[end + 1..];
    }
    output.push_str(rest);
    output
}

fn apply_declaration(
    style: &mut ComputedStyle,
    priorities: &mut HashMap<String, Priority>,
    declaration: &Declaration,
    priority: Priority,
) {
    let property = declaration.property.to_ascii_lowercase();
    if property.is_empty() || property.starts_with("--") && declaration.value.is_empty() {
        return;
    }
    if priorities.get(&property).is_none_or(|current| priority >= *current) {
        style.set(property.clone(), declaration.value.clone());
        priorities.insert(property, priority);
    }
}

fn parse_inline_declarations(source: &str) -> Result<Vec<Declaration>, CssError> {
    let tokens = tokenize(source)?;
    Ok(parse_declarations(&tokens))
}

fn matches_selector(
    document: &HtmlDocument,
    id: NodeId,
    selector: &ComplexSelector,
    budget: &mut MatchBudget,
) -> bool {
    let Some(last) = selector.compounds.len().checked_sub(1) else {
        return false;
    };
    matches_from_right(document, id, selector, last, budget)
}

fn matches_from_right(
    document: &HtmlDocument,
    id: NodeId,
    selector: &ComplexSelector,
    compound_index: usize,
    budget: &mut MatchBudget,
) -> bool {
    // Each recursive step and each compound check consumes work. Once the
    // budget is gone, matching fails closed instead of running unbounded.
    if compound_index >= MAX_SELECTOR_COMPOUNDS || !budget.tick() {
        return false;
    }
    let Some(node) = document.node(id) else { return false };
    if !matches_compound(document, id, node.kind(), &selector.compounds[compound_index], budget) {
        return false;
    }
    if compound_index == 0 {
        return true;
    }
    match selector.combinators[compound_index - 1] {
        Combinator::Child => node.parent().is_some_and(|parent| {
            matches_from_right(document, parent, selector, compound_index - 1, budget)
        }),
        Combinator::Descendant => {
            let mut parent = node.parent();
            while let Some(parent_id) = parent {
                if budget.exhausted() {
                    return false;
                }
                if matches_from_right(document, parent_id, selector, compound_index - 1, budget) {
                    return true;
                }
                parent = document.node(parent_id).and_then(|parent_node| parent_node.parent());
            }
            false
        }
        Combinator::AdjacentSibling => {
            previous_element_sibling(document, id).is_some_and(|sibling| {
                matches_from_right(document, sibling, selector, compound_index - 1, budget)
            })
        }
        Combinator::GeneralSibling => {
            let mut sibling = previous_element_sibling(document, id);
            while let Some(id) = sibling {
                if budget.exhausted() {
                    return false;
                }
                if matches_from_right(document, id, selector, compound_index - 1, budget) {
                    return true;
                }
                sibling = previous_element_sibling(document, id);
            }
            false
        }
    }
}

fn previous_element_sibling(document: &HtmlDocument, id: NodeId) -> Option<NodeId> {
    let parent = document.node(id)?.parent()?;
    let siblings = document.node(parent)?.children();
    let position = siblings.iter().position(|sibling| *sibling == id)?;
    siblings[..position].iter().copied().rev().find(|sibling| {
        document.node(*sibling).is_some_and(|node| matches!(node.kind(), NodeKind::Element { .. }))
    })
}

fn matches_compound(
    document: &HtmlDocument,
    id: NodeId,
    kind: &NodeKind,
    selector: &CompoundSelector,
    budget: &mut MatchBudget,
) -> bool {
    if !budget.tick() {
        return false;
    }
    let NodeKind::Element { tag_name, attributes } = kind else {
        return false;
    };
    if !selector.universal
        && selector.tag.as_ref().is_some_and(|tag| !tag.eq_ignore_ascii_case(tag_name))
    {
        return false;
    }
    let attr = |name: &str| attributes.iter().find(|attribute| attribute.name == name);
    if selector
        .id
        .as_ref()
        .is_some_and(|expected| attr("id").is_none_or(|actual| actual.value != *expected))
    {
        return false;
    }
    if selector.classes.iter().any(|expected| {
        attr("class").is_none_or(|actual| {
            !actual.value.split_ascii_whitespace().any(|part| part == expected)
        })
    }) {
        return false;
    }
    if selector.attributes.iter().any(|expected| {
        let Some(actual) = attr(&expected.name) else {
            return true;
        };
        let Some(value) = &expected.value else {
            return false;
        };
        let haystack = if expected.case_insensitive {
            actual.value.to_ascii_lowercase()
        } else {
            actual.value.clone()
        };
        let needle =
            if expected.case_insensitive { value.to_ascii_lowercase() } else { value.clone() };
        let matches = match expected.operator {
            AttributeOperator::Exists => true,
            AttributeOperator::Equals => haystack == needle,
            AttributeOperator::Includes => {
                haystack.split_ascii_whitespace().any(|part| part == needle)
            }
            AttributeOperator::DashMatch => {
                haystack == needle
                    || haystack.starts_with(&needle)
                        && haystack.as_bytes().get(needle.len()) == Some(&b'-')
            }
            AttributeOperator::Prefix => haystack.starts_with(&needle),
            AttributeOperator::Suffix => haystack.ends_with(&needle),
            AttributeOperator::Substring => haystack.contains(&needle),
        };
        !matches
    }) {
        return false;
    }
    selector.pseudos.iter().all(|pseudo| match pseudo {
        PseudoClass::Root => {
            document.node(id).and_then(|node| node.parent()).is_some_and(|parent| {
                document.node(parent).is_some_and(|node| matches!(node.kind(), NodeKind::Document))
            })
        }
        PseudoClass::FirstChild => {
            matches!(sibling_position(document, id, None, budget), Some((1, _)))
        }
        PseudoClass::LastChild => {
            matches!(sibling_position(document, id, None, budget), Some((index, total)) if index == total)
        }
        PseudoClass::OnlyChild => {
            matches!(sibling_position(document, id, None, budget), Some((1, 1)))
        }
        PseudoClass::FirstOfType => {
            matches!(sibling_position(document, id, Some(tag_name), budget), Some((1, _)))
        }
        PseudoClass::LastOfType => {
            matches!(sibling_position(document, id, Some(tag_name), budget), Some((index, total)) if index == total)
        }
        PseudoClass::Empty => document.node(id).is_some_and(|node| {
            node.children().iter().all(|child| {
                document
                    .node(*child)
                    .is_none_or(|child_node| matches!(child_node.kind(), NodeKind::Comment(_)))
            })
        }),
        PseudoClass::AnyLink => tag_name == "a" && attr("href").is_some(),
        PseudoClass::NthChild(nth) => sibling_position(document, id, None, budget)
            .is_some_and(|(index, _)| nth.matches(index)),
        PseudoClass::NthLastChild(nth) => sibling_position(document, id, None, budget)
            .is_some_and(|(index, total)| nth.matches(total - index + 1)),
        PseudoClass::Not(inner) => !matches_compound(document, id, kind, inner, budget),
    })
}

/// One-based position among element siblings, optionally restricted to a tag.
/// Each sibling visited consumes one unit of the shared matching budget; an
/// exhausted budget aborts the scan cleanly with no result.
fn sibling_position(
    document: &HtmlDocument,
    id: NodeId,
    tag: Option<&str>,
    budget: &mut MatchBudget,
) -> Option<(usize, usize)> {
    let parent = document.node(id)?.parent()?;
    let mut position = 0usize;
    let mut total = 0usize;
    for sibling in document.node(parent)?.children() {
        // Charge before inspecting the sibling so the budget is never exceeded
        // by a large sibling list.
        if !budget.tick() {
            return None;
        }
        let NodeKind::Element { tag_name, .. } = document.node(*sibling)?.kind() else {
            continue;
        };
        if tag.is_some_and(|expected| !expected.eq_ignore_ascii_case(tag_name)) {
            continue;
        }
        total += 1;
        if *sibling == id {
            position = total;
        }
    }
    (position > 0).then_some((position, total))
}

#[cfg(test)]
mod tests {
    use super::{
        ComputedStyle, ComputedStyleMap, MATCH_WORK_BUDGET, MAX_SELECTOR_COMPOUNDS, Specificity,
        StyleEngine, Stylesheet, TokenKind, tokenize,
    };
    use bweb_html::{HtmlDocument, NodeKind};
    use std::time::{Duration, Instant};

    #[test]
    fn lexer_recognizes_comments_strings_and_css_punctuation() {
        let tokens = tokenize("/* note */ .card { color: \"red\"; }").expect("tokens");
        assert!(tokens.iter().any(|token| token.kind == TokenKind::String));
        assert!(tokens.iter().any(|token| token.kind == TokenKind::OpenBrace));
        assert!(!tokens.iter().any(|token| token.text.contains("note")));
    }

    #[test]
    fn parses_rules_and_important_declarations() {
        let sheet = Stylesheet::parse("p, .notice { color: red !important; margin: 0; }")
            .expect("stylesheet");
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.rules[0].selectors.len(), 2);
        assert!(sheet.rules[0].declarations[0].important);
        assert_eq!(sheet.rules[0].declarations[1].property, "margin");
    }

    #[test]
    fn computes_specificity_and_descendant_child_matches() {
        let document = HtmlDocument::parse(
            r#"<main id="app"><section class="panel"><p class="notice">Hello</p></section></main>"#,
        );
        let sheet = Stylesheet::parse(
            "p { color: black; } #app .panel > p.notice { color: red; font-size: 20px; }",
        )
        .expect("stylesheet");
        let styles = StyleEngine::new().compute(&document, &[sheet]);
        let paragraph = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "p")
                    .then_some(id)
            })
            .expect("paragraph");
        let style = styles.get(paragraph).expect("computed style");
        assert_eq!(style.get("color"), Some("red"));
        assert_eq!(style.get("font-size"), Some("20px"));
    }

    #[test]
    fn inline_style_wins_over_normal_author_rules_and_important_wins_over_inline_normal() {
        let document = HtmlDocument::parse(r#"<p style="color: blue; margin: 2px">Hello</p>"#);
        let sheet =
            Stylesheet::parse("p { color: red; margin: 0 !important; }").expect("stylesheet");
        let styles = StyleEngine::new().compute(&document, &[sheet]);
        let id = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "p")
                    .then_some(id)
            })
            .expect("paragraph");
        let style = styles.get(id).expect("computed style");
        assert_eq!(style.get("color"), Some("blue"));
        assert_eq!(style.get("margin"), Some("0"));
    }

    #[test]
    fn computes_styles_for_text_nodes_too() {
        let document = HtmlDocument::parse("<p>Text</p>");
        let sheet = Stylesheet::parse("p { color: purple; }").expect("stylesheet");
        let styles = StyleEngine::new().compute(&document, &[sheet]);
        let text_id = document
            .iter()
            .find_map(|(id, node)| matches!(node.kind(), NodeKind::Text(_)).then_some(id))
            .expect("text node");
        assert_eq!(styles.get(text_id).and_then(|style| style.get("color")), Some("purple"));
        assert!(!styles.is_empty());
    }

    #[test]
    fn reports_unterminated_css_constructs() {
        assert!(Stylesheet::parse("p { color: red;").is_err());
        assert!(tokenize("/* never closed").is_err());
    }

    #[test]
    fn exposes_stable_computed_style_values() {
        let style = ComputedStyle::default();
        assert_eq!(style.get("color"), None);
        assert_eq!(Specificity(1, 0, 0), Specificity(1, 0, 0));
    }

    fn by_id(document: &HtmlDocument, id: &str) -> bweb_html::NodeId {
        document
            .iter()
            .find_map(|(node_id, node)| match node.kind() {
                NodeKind::Element { attributes, .. } => attributes
                    .iter()
                    .find(|attribute| attribute.name == "id" && attribute.value == id)
                    .map(|_| node_id),
                _ => None,
            })
            .expect("element with id")
    }

    #[test]
    fn matches_sibling_combinators() {
        let document =
            HtmlDocument::parse("<main><p id='a'>A</p><p id='b'>B</p><p id='c'>C</p></main>");
        let sheet = Stylesheet::parse("#a + p { color: red; } #a ~ p { background-color: blue; }")
            .expect("stylesheet");
        let styles = StyleEngine::new().compute(&document, &[sheet]);
        let b = styles.get(by_id(&document, "b")).expect("b styles");
        let c = styles.get(by_id(&document, "c")).expect("c styles");
        assert_eq!(b.get("color"), Some("red"));
        assert_eq!(b.get("background-color"), Some("blue"));
        assert_ne!(c.get("color"), Some("red"));
        assert_eq!(c.get("background-color"), Some("blue"));
    }

    #[test]
    fn sibling_selectors_work_with_implied_end_tags() {
        let document = HtmlDocument::parse("<main><p>one<p>two<p>three</main>");
        let sheet = Stylesheet::parse("p + p { color: red; } p:first-child { color: blue; }")
            .expect("stylesheet");
        let styles = StyleEngine::new().compute(&document, &[sheet]);
        let paragraphs: Vec<_> = document
            .iter()
            .filter_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "p")
                    .then_some(id)
            })
            .collect();
        assert_eq!(paragraphs.len(), 3, "omitted </p> must create siblings");
        assert_eq!(styles.get(paragraphs[0]).unwrap().get("color"), Some("blue"));
        assert_eq!(styles.get(paragraphs[1]).unwrap().get("color"), Some("red"));
        assert_eq!(styles.get(paragraphs[2]).unwrap().get("color"), Some("red"));
    }

    #[test]
    fn matches_structural_and_negation_pseudos() {
        let document = HtmlDocument::parse("<ul><li class='x'>1</li><li>2</li><li>3</li></ul>");
        let sheet = Stylesheet::parse(
            "li:first-child { color: red; } li:nth-child(2) { font-size: 20px; } li:not(.x) { border-width: 1px; }",
        )
        .expect("stylesheet");
        let styles = StyleEngine::new().compute(&document, &[sheet]);
        let items: Vec<_> = document
            .iter()
            .filter_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "li")
                    .then_some(id)
            })
            .collect();
        assert_eq!(styles.get(items[0]).unwrap().get("color"), Some("red"));
        assert_eq!(styles.get(items[1]).unwrap().get("font-size"), Some("20px"));
        assert_eq!(styles.get(items[0]).unwrap().get("border-width"), None);
        assert_eq!(styles.get(items[1]).unwrap().get("border-width"), Some("1px"));
    }

    #[test]
    fn matches_attribute_operators() {
        let document = HtmlDocument::parse(
            "<main><a id='one' data-x='foo bar' lang='en-US' href='https://example.test/a'></a><a id='two' data-x='baz' lang='fr' href='http://other.test'></a></main>",
        );
        let sheet = Stylesheet::parse(
            "[data-x~=bar] { color: red; } [lang|=en] { font-size: 20px; } [href^=https] { border-width: 1px; } [href$=a] { margin: 2px; } [href*=example] { padding: 3px; }",
        )
        .expect("stylesheet");
        let styles = StyleEngine::new().compute(&document, &[sheet]);
        let one = styles.get(by_id(&document, "one")).expect("one styles");
        assert_eq!(one.get("color"), Some("red"));
        assert_eq!(one.get("font-size"), Some("20px"));
        assert_eq!(one.get("border-width"), Some("1px"));
        assert_eq!(one.get("margin"), Some("2px"));
        assert_eq!(one.get("padding"), Some("3px"));
    }

    #[test]
    fn media_queries_are_evaluated_against_the_viewport() {
        let document = HtmlDocument::parse("<html><body><p>x</p></body></html>");
        let sheet = Stylesheet::parse(
            "@media (max-width: 600px) { p { color: red; } } @media screen and (min-width: 801px) { p { color: blue; } }",
        )
        .expect("stylesheet");
        let paragraph = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "p")
                    .then_some(id)
            })
            .expect("paragraph");
        let narrow = StyleEngine::new().compute_with_viewport(
            &document,
            std::slice::from_ref(&sheet),
            400.0,
            800.0,
        );
        let wide = StyleEngine::new().compute_with_viewport(&document, &[sheet], 1000.0, 800.0);
        assert_eq!(narrow.get(paragraph).unwrap().get("color"), Some("red"));
        assert_eq!(wide.get(paragraph).unwrap().get("color"), Some("blue"));
    }

    #[test]
    fn collects_generated_styles_from_pseudo_element_rules() {
        let document = HtmlDocument::parse("<html><body><p>x</p></body></html>");
        let sheet = Stylesheet::parse(
            "p { color: black; } p::before { content: \"→\"; color: red; } p::after { content: \"!\" }",
        )
        .expect("stylesheet");
        let paragraph = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "p")
                    .then_some(id)
            })
            .expect("paragraph");
        let (base, generated) =
            StyleEngine::new().compute_generated(&document, &[sheet], 800.0, 600.0);
        assert_eq!(base.get(paragraph).unwrap().get("color"), Some("black"));
        let before = generated.get(&(paragraph, super::PseudoElement::Before)).expect("before");
        assert_eq!(before.get("content"), Some("\"→\""));
        assert_eq!(before.get("color"), Some("red"));
        let after = generated.get(&(paragraph, super::PseudoElement::After)).expect("after");
        assert_eq!(after.get("content"), Some("\"!\""));
        assert_eq!(after.get("color"), Some("black"), "pseudo styles inherit");
    }

    #[test]
    fn expands_shorthands_but_keeps_explicit_longhands() {
        let document = HtmlDocument::parse(
            "<html><head><style>p { margin: 10px 20px; margin-top: 30px; padding: 4px; background: red; border: 1px solid blue; border-left-color: green; font: italic bold 20px/1.4 serif; }</style></head><body><p>x</p></body></html>",
        );
        let sheets: Vec<_> = document
            .inline_stylesheets()
            .iter()
            .map(|source| Stylesheet::parse(source).expect("inline stylesheet"))
            .collect();
        let styles = StyleEngine::new().compute(&document, &sheets);
        let paragraph = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "p")
                    .then_some(id)
            })
            .expect("paragraph");
        let style = styles.get(paragraph).expect("computed style");
        assert_eq!(style.get("margin-top"), Some("30px"));
        assert_eq!(style.get("margin-right"), Some("20px"));
        assert_eq!(style.get("padding-left"), Some("4px"));
        assert_eq!(style.get("background-color"), Some("red"));
        assert_eq!(style.get("border-width"), Some("1px"));
        assert_eq!(style.get("border-style"), Some("solid"));
        assert_eq!(style.get("border-color"), Some("blue"));
        assert_eq!(style.get("border-left-color"), Some("green"));
        assert_eq!(style.get("font-size"), Some("20px"));
        assert_eq!(style.get("line-height"), Some("1.4"));
        assert_eq!(style.get("font-style"), Some("italic"));
        assert_eq!(style.get("font-weight"), Some("bold"));
        assert_eq!(style.get("font-family"), Some("serif"));
    }

    #[test]
    fn expands_the_background_shorthand_into_all_longhands() {
        let document = HtmlDocument::parse(
            "<html><head><style>\
             p { background: green linear-gradient(to right, red, blue) no-repeat center / cover padding-box border-box; }\
             a { background: red; background-image: none; }\
             div { background: url(a.png) no-repeat, linear-gradient(red, blue); }\
             </style></head><body><p>x</p><a>y</a><div>z</div></body></html>",
        );
        let sheets: Vec<_> = document
            .inline_stylesheets()
            .iter()
            .map(|source| Stylesheet::parse(source).expect("inline stylesheet"))
            .collect();
        let styles = StyleEngine::new().compute(&document, &sheets);
        let by_tag = |tag: &str| {
            document
                .iter()
                .find_map(|(id, node)| {
                    matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == tag)
                        .then_some(id)
                })
                .and_then(|id| styles.get(id))
                .expect("computed style")
        };

        let paragraph = by_tag("p");
        assert_eq!(paragraph.get("background-color"), Some("green"));
        assert_eq!(paragraph.get("background-image"), Some("linear-gradient(to right, red, blue)"));
        assert_eq!(paragraph.get("background-repeat"), Some("no-repeat"));
        assert_eq!(paragraph.get("background-position"), Some("center"));
        assert_eq!(paragraph.get("background-size"), Some("cover"));
        assert_eq!(paragraph.get("background-origin"), Some("padding-box"));
        assert_eq!(paragraph.get("background-clip"), Some("border-box"));

        let anchor = by_tag("a");
        assert_eq!(anchor.get("background-color"), Some("red"));
        assert_eq!(anchor.get("background-image"), Some("none"));

        let division = by_tag("div");
        assert_eq!(
            division.get("background-image"),
            Some("url(a.png), linear-gradient(red, blue)")
        );
        assert_eq!(division.get("background-repeat"), Some("no-repeat, repeat"));
        assert_eq!(division.get("background-color"), Some("transparent"));
    }

    #[test]
    fn layers_lose_to_unlayered_but_keep_their_order() {
        let document = HtmlDocument::parse(
            "<html><head><style>\
             @layer first { p { color: red } } \
             @layer second { p { color: blue } } \
             p { color: green } \
             </style></head><body><p>x</p></body></html>",
        );
        let sheets: Vec<_> = document
            .inline_stylesheets()
            .iter()
            .map(|source| Stylesheet::parse(source).expect("inline stylesheet"))
            .collect();
        let styles = StyleEngine::new().compute(&document, &sheets);
        let paragraph = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "p")
                    .then_some(id)
            })
            .expect("paragraph");
        assert_eq!(styles.get(paragraph).expect("style").get("color"), Some("green"));
    }

    #[test]
    fn layered_important_reverses_the_layer_order() {
        let document = HtmlDocument::parse(
            "<html><head><style>\
             @layer first { p { color: red !important } } \
             @layer second { p { color: blue !important } } \
             p { color: green } \
             </style></head><body><p>x</p></body></html>",
        );
        let sheets: Vec<_> = document
            .inline_stylesheets()
            .iter()
            .map(|source| Stylesheet::parse(source).expect("inline stylesheet"))
            .collect();
        let styles = StyleEngine::new().compute(&document, &sheets);
        let paragraph = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "p")
                    .then_some(id)
            })
            .expect("paragraph");
        assert_eq!(styles.get(paragraph).expect("style").get("color"), Some("red"));
    }

    #[test]
    fn expands_flex_and_gap_shorthands() {
        let document = HtmlDocument::parse(
            "<html><head><style>main { display:flex; flex:1 2 auto; gap:10px 4px } span { flex:1 }</style></head><body><main><span>x</span></main></body></html>",
        );
        let sheets: Vec<_> = document
            .inline_stylesheets()
            .iter()
            .map(|source| Stylesheet::parse(source).expect("inline stylesheet"))
            .collect();
        let styles = StyleEngine::new().compute(&document, &sheets);
        let by_tag = |tag: &str| {
            document
                .iter()
                .find_map(|(id, node)| {
                    matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == tag)
                        .then_some(id)
                })
                .and_then(|id| styles.get(id))
                .expect("computed style")
        };
        let main = by_tag("main");
        assert_eq!(main.get("display"), Some("flex"));
        assert_eq!(main.get("flex-grow"), Some("1"));
        assert_eq!(main.get("flex-shrink"), Some("2"));
        assert_eq!(main.get("flex-basis"), Some("auto"));
        assert_eq!(main.get("row-gap"), Some("10px"));
        assert_eq!(main.get("column-gap"), Some("4px"));

        let span = by_tag("span");
        assert_eq!(span.get("flex-grow"), Some("1"));
        assert_eq!(span.get("flex-shrink"), Some("1"));
        assert_eq!(span.get("flex-basis"), Some("0"));
    }

    #[test]
    fn deeply_nested_at_rules_are_rejected_without_stack_overflow() {
        // Thousands of nested @media blocks must be rejected by the explicit
        // depth guard, long before the native stack is exhausted.
        let mut media = String::new();
        for _ in 0..5_000 {
            media.push_str("@media screen { ");
        }
        media.push_str("p { color: red }");
        for _ in 0..5_000 {
            media.push_str(" }");
        }
        let error = Stylesheet::parse(&media).expect_err("deep @media must fail");
        assert!(error.message.contains("nesting depth"), "unexpected error: {}", error.message);

        let mut layers = String::new();
        for index in 0..5_000 {
            layers.push_str(&format!("@layer L{index} {{ "));
        }
        layers.push_str("p { color: red }");
        for _ in 0..5_000 {
            layers.push_str(" }");
        }
        let error = Stylesheet::parse(&layers).expect_err("deep @layer must fail");
        assert!(error.message.contains("nesting depth"), "unexpected error: {}", error.message);
    }

    #[test]
    fn structural_pseudo_classes_respect_the_match_budget() {
        let markup = format!("<main><p id='first'></p>{}</main>", "<p></p>".repeat(5_000));
        let document = HtmlDocument::parse(markup);
        let sheet = Stylesheet::parse(
            "p:first-child { color: red } \
             p:last-child { color: green } \
             p:first-of-type { color: blue } \
             p:nth-child(2) { color: orange } \
             p:nth-last-child(1) { color: purple }",
        )
        .expect("structural pseudo rules");
        let first = document.element_by_id("first").expect("first paragraph");

        // A tiny budget must stop the sibling scans: none of the structural
        // rules may be applied, even though the first paragraph matches them.
        let starved = StyleEngine::new().compute_with_match_budget(
            &document,
            std::slice::from_ref(&sheet),
            800.0,
            600.0,
            // Four non-matching ancestors cost 4 x 5 rules x 2 units = 40,
            // then the first paragraph costs 2 more: the scan starts with a
            // single unit left.
            43,
        );
        let color = |styles: &ComputedStyleMap| {
            styles.get(first).and_then(|style| style.get("color")).map(str::to_owned)
        };
        assert_ne!(color(&starved), Some("red".to_owned()));

        // With a normal budget the same page still styles the first paragraph.
        let normal = StyleEngine::new().compute_with_match_budget(
            &document,
            std::slice::from_ref(&sheet),
            800.0,
            600.0,
            MATCH_WORK_BUDGET,
        );
        // The first paragraph matches both :first-child and :first-of-type;
        // the later equal-specificity rule wins.
        assert!(matches!(color(&normal), Some(color) if color == "red" || color == "blue"));
    }

    #[test]
    fn match_budget_bounds_expensive_selector_work() {
        let mut document = HtmlDocument::parse("<main id='root'></main>");
        let mut deepest = document.element_by_id("root").expect("root");
        for _ in 0..120 {
            let child = document.create_element("div");
            assert!(document.append_child(deepest, child));
            deepest = child;
        }
        // 64 descendants whose first component never matches: the matcher
        // backtracks over every ancestor at every node.
        let sheet = Stylesheet::parse(&format!("{} div {{ color: red }}", "div ".repeat(63)))
            .expect("64-compound selector");

        // A one-unit budget stops matching immediately; no declaration is applied.
        let starved = StyleEngine::new().compute_with_match_budget(
            &document,
            std::slice::from_ref(&sheet),
            800.0,
            600.0,
            1,
        );
        assert_ne!(starved.styles.get(&deepest).and_then(|s| s.get("color")), Some("red"));

        // A normal budget keeps the computation bounded even on the deep DOM.
        let start = Instant::now();
        let normal = StyleEngine::new().compute_with_match_budget(
            &document,
            &[sheet],
            800.0,
            600.0,
            MATCH_WORK_BUDGET,
        );
        assert!(start.elapsed() < Duration::from_secs(3), "matching took too long");
        assert!(normal.styles.contains_key(&deepest));
    }

    #[test]
    fn long_combinator_chains_are_rejected_without_stack_overflow() {
        for combinator in [" ", " > ", " + ", " ~ "] {
            let selector = vec!["div"; 5_000].join(combinator);
            let source = format!("{selector} {{ color: red }}");
            let error = Stylesheet::parse(&source).expect_err("long selector must fail");
            assert!(
                error.message.contains("selector length"),
                "unexpected error for {combinator:?}: {}",
                error.message
            );
        }
        // A selector just under the limit still parses and matches.
        let short = vec!["div"; MAX_SELECTOR_COMPOUNDS].join(" ");
        Stylesheet::parse(&format!("{short} {{ color: red }}"))
            .expect("selector at the limit is accepted");
    }

    #[test]
    fn deeply_nested_negation_selectors_are_rejected() {
        let mut selector = String::from("p");
        for _ in 0..1_000 {
            selector = format!(":not({selector})");
        }
        let source = format!("{selector} {{ color: red }}");
        let error = Stylesheet::parse(&source).expect_err("deep :not must fail");
        assert!(error.message.contains("nesting depth"), "unexpected error: {}", error.message);
    }

    #[test]
    fn substitutes_css_custom_properties_with_fallbacks() {
        let document = HtmlDocument::parse("<html><body><p>x</p></body></html>");
        let sheet = Stylesheet::parse(
            ":root { --brand: teal; } p { color: var(--brand); border-color: var(--missing, red); }",
        )
        .expect("stylesheet");
        let styles = StyleEngine::new().compute(&document, &[sheet]);
        let paragraph = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "p")
                    .then_some(id)
            })
            .expect("paragraph");
        let style = styles.get(paragraph).expect("computed style");
        assert_eq!(style.get("color"), Some("teal"));
        assert_eq!(style.get("border-color"), Some("red"));
    }
}
