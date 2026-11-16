//! Initial CSS lexer, rule parser, selector matcher, cascade, and computed styles.
//!
//! This is an intentionally documented CSS subset, not a complete CSS Syntax or
//! Selectors implementation. Unsupported at-rules are skipped safely.

use bweb_html::{HtmlDocument, NodeId, NodeKind};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashMap},
    error::Error,
    fmt,
};

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
}

/// Parsed stylesheet containing supported qualified rules.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stylesheet {
    pub rules: Vec<StyleRule>,
}

impl Stylesheet {
    /// Parses supported style rules; unsupported at-rules are skipped.
    pub fn parse(source: &str) -> Result<Self, CssError> {
        let tokens = tokenize(source)?;
        let mut rules = Vec::new();
        let mut index = 0;
        while index < tokens.len() {
            skip_whitespace(&tokens, &mut index);
            if index >= tokens.len() {
                break;
            }
            if tokens[index].kind == TokenKind::AtKeyword {
                index += 1;
                while index < tokens.len()
                    && tokens[index].kind != TokenKind::Semicolon
                    && tokens[index].kind != TokenKind::OpenBrace
                {
                    index += 1;
                }
                if index < tokens.len() && tokens[index].kind == TokenKind::Semicolon {
                    index += 1;
                } else if index < tokens.len() {
                    index = skip_block(&tokens, index)?;
                }
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
                rules.push(StyleRule { selectors, declarations, source_order: rules.len() });
            }
            index += 1;
        }
        Ok(Self { rules })
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComplexSelector {
    compounds: Vec<CompoundSelector>,
    combinators: Vec<Combinator>,
    pub specificity: Specificity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Combinator {
    Descendant,
    Child,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CompoundSelector {
    tag: Option<String>,
    universal: bool,
    id: Option<String>,
    classes: Vec<String>,
    attributes: Vec<AttributeSelector>,
    pseudos: Vec<PseudoClass>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AttributeSelector {
    name: String,
    value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PseudoClass {
    Root,
    FirstChild,
    LastChild,
    Empty,
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
        let mut consumed_space = false;
        while index < chars.len() && chars[index].is_ascii_whitespace() {
            index += 1;
            consumed_space = true;
        }
        if index >= chars.len() {
            break;
        }
        if chars[index] == '>' {
            if compounds.is_empty() || combinators.len() + 1 != compounds.len() {
                return Err(CssError::new(0, "misplaced child combinator"));
            }
            index += 1;
            while index < chars.len() && chars[index].is_ascii_whitespace() {
                index += 1;
            }
            if index >= chars.len() {
                return Err(CssError::new(0, "selector cannot end with a combinator"));
            }
            let compound = parse_compound(&chars, &mut index, &mut specificity)?;
            combinators.push(Combinator::Child);
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
        let compound = parse_compound(&chars, &mut index, &mut specificity)?;
        compounds.push(compound);
    }
    if compounds.is_empty() || combinators.len() + 1 != compounds.len() {
        return Err(CssError::new(0, "invalid selector"));
    }
    Ok(ComplexSelector { compounds, combinators, specificity })
}

fn parse_compound(
    chars: &[char],
    index: &mut usize,
    specificity: &mut Specificity,
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
                let value = if *index < chars.len() && chars[*index] == '=' {
                    *index += 1;
                    skip_char_space(chars, index);
                    let value = read_attribute_value(chars, index);
                    skip_char_space(chars, index);
                    Some(value)
                } else {
                    None
                };
                if *index >= chars.len() || chars[*index] != ']' {
                    return Err(CssError::new(0, "unterminated attribute selector"));
                }
                *index += 1;
                compound.attributes.push(AttributeSelector { name, value });
                specificity.1 += 1;
                consumed = true;
            }
            ':' => {
                *index += 1;
                let name = read_ident(chars, index).to_ascii_lowercase();
                let pseudo = match name.as_str() {
                    "root" => PseudoClass::Root,
                    "first-child" => PseudoClass::FirstChild,
                    "last-child" => PseudoClass::LastChild,
                    "empty" => PseudoClass::Empty,
                    _ => return Err(CssError::new(0, format!("unsupported pseudo-class :{name}"))),
                };
                compound.pseudos.push(pseudo);
                specificity.1 += 1;
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

    fn set(&mut self, property: String, value: String) {
        self.properties.insert(property, value);
    }

    fn initial() -> Self {
        let mut style = Self::default();
        style.set("display".into(), "inline".into());
        style.set("position".into(), "static".into());
        style.set("color".into(), "black".into());
        style.set("background-color".into(), "transparent".into());
        style.set("font-size".into(), "16px".into());
        style.set("font-family".into(), "serif".into());
        style.set("font-weight".into(), "normal".into());
        style.set("font-style".into(), "normal".into());
        style.set("line-height".into(), "normal".into());
        style.set("visibility".into(), "visible".into());
        style.set("white-space".into(), "normal".into());
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Priority {
    important: bool,
    inline: bool,
    specificity: Specificity,
    order: usize,
}

impl Ord for Priority {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.important, self.inline, self.specificity, self.order).cmp(&(
            other.important,
            other.inline,
            other.specificity,
            other.order,
        ))
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

    /// Computes styles for every DOM NodeId. Text/comment nodes inherit their
    /// parent's inherited properties; layout-dependent values are not resolved.
    pub fn compute(&self, document: &HtmlDocument, stylesheets: &[Stylesheet]) -> ComputedStyleMap {
        let mut result = ComputedStyleMap::default();
        let mut priorities: HashMap<NodeId, HashMap<String, Priority>> = HashMap::new();
        for (id, node) in document.iter() {
            let mut computed = node
                .parent()
                .and_then(|parent| result.styles.get(&parent))
                .map(ComputedStyle::inherited_from)
                .unwrap_or_else(ComputedStyle::initial);
            if let NodeKind::Element { attributes, .. } = node.kind() {
                for (sheet_index, sheet) in stylesheets.iter().enumerate() {
                    for rule in &sheet.rules {
                        let best = rule
                            .selectors
                            .iter()
                            .filter(|selector| matches_selector(document, id, selector))
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
                                        inline: false,
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
                                    inline: true,
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
        result
    }
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

fn matches_selector(document: &HtmlDocument, id: NodeId, selector: &ComplexSelector) -> bool {
    let Some(last) = selector.compounds.len().checked_sub(1) else {
        return false;
    };
    matches_from_right(document, id, selector, last)
}

fn matches_from_right(
    document: &HtmlDocument,
    id: NodeId,
    selector: &ComplexSelector,
    compound_index: usize,
) -> bool {
    let Some(node) = document.node(id) else { return false };
    if !matches_compound(document, id, node.kind(), &selector.compounds[compound_index]) {
        return false;
    }
    if compound_index == 0 {
        return true;
    }
    match selector.combinators[compound_index - 1] {
        Combinator::Child => node.parent().is_some_and(|parent| {
            matches_from_right(document, parent, selector, compound_index - 1)
        }),
        Combinator::Descendant => {
            let mut parent = node.parent();
            while let Some(parent_id) = parent {
                if matches_from_right(document, parent_id, selector, compound_index - 1) {
                    return true;
                }
                parent = document.node(parent_id).and_then(|parent_node| parent_node.parent());
            }
            false
        }
    }
}

fn matches_compound(
    document: &HtmlDocument,
    id: NodeId,
    kind: &NodeKind,
    selector: &CompoundSelector,
) -> bool {
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
        attr(&expected.name).is_none_or(|actual| {
            expected.value.as_ref().is_some_and(|value| actual.value != *value)
        })
    }) {
        return false;
    }
    selector.pseudos.iter().all(|pseudo| match pseudo {
        PseudoClass::Root => {
            document.node(id).and_then(|node| node.parent()).is_some_and(|parent| {
                document.node(parent).is_some_and(|node| matches!(node.kind(), NodeKind::Document))
            })
        }
        PseudoClass::FirstChild => is_element_sibling_position(document, id, true),
        PseudoClass::LastChild => is_element_sibling_position(document, id, false),
        PseudoClass::Empty => document.node(id).is_some_and(|node| {
            node.children().iter().all(|child| {
                document
                    .node(*child)
                    .is_none_or(|child_node| matches!(child_node.kind(), NodeKind::Comment(_)))
            })
        }),
    })
}

fn is_element_sibling_position(document: &HtmlDocument, id: NodeId, first: bool) -> bool {
    let Some(node) = document.node(id) else { return false };
    let Some(parent) = node.parent().and_then(|parent| document.node(parent)) else {
        return false;
    };
    let siblings: Vec<NodeId> = parent
        .children()
        .iter()
        .copied()
        .filter(|sibling| {
            document
                .node(*sibling)
                .is_some_and(|sibling_node| matches!(sibling_node.kind(), NodeKind::Element { .. }))
        })
        .collect();
    if first { siblings.first() == Some(&id) } else { siblings.last() == Some(&id) }
}

#[cfg(test)]
mod tests {
    use super::{ComputedStyle, Specificity, StyleEngine, Stylesheet, TokenKind, tokenize};
    use bweb_html::{HtmlDocument, NodeKind};

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
}
