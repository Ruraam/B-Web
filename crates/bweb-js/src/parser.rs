//! Recursive-descent parser for the B-Web JavaScript subset.

use crate::{
    ast::{BinaryOp, Expr, LogicalOp, Stmt, UnaryOp, VarKind},
    lexer::{MAX_PARSE_TOKENS, Token, TokenKind, tokenize_with_limit},
};
use std::{cell::Cell, rc::Rc};

/// Maximum nesting depth of the recursive-descent parser. Resource size limits
/// do not bound nesting, so this guard prevents stack exhaustion on deeply
/// nested source produced by a remote script.
const MAX_PARSE_DEPTH: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ParseError {}

/// Decrements the shared nesting counter when a recursive parse frame exits.
struct DepthGuard(Rc<Cell<usize>>);

impl Drop for DepthGuard {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1);
    }
}

struct Parser {
    tokens: Vec<Token>,
    position: usize,
    depth: Rc<Cell<usize>>,
    /// Tokens still available to this parse call. Template interpolations are
    /// parsed recursively and draw from the same allowance.
    token_budget: Rc<Cell<usize>>,
}

/// Parses a source string into a list of statements.
pub fn parse(source: &str) -> Result<Vec<Stmt>, ParseError> {
    let token_budget = Rc::new(Cell::new(MAX_PARSE_TOKENS));
    parse_with_depth(source, 0, &token_budget)
}

/// Parses a source string while drawing tokens from a caller-owned budget.
/// Used by `Runtime::eval` to enforce a cumulative budget across every script
/// of a page. The budget is debited even when parsing fails, so a hostile
/// script cannot repeat failed parses for free.
pub(crate) fn parse_with_token_budget(
    source: &str,
    token_budget: Rc<Cell<usize>>,
) -> Result<Vec<Stmt>, ParseError> {
    parse_with_depth(source, 0, &token_budget)
}

/// Internal entry point that carries the nesting depth and token budget across
/// template interpolations, which are parsed as nested sources.
fn parse_with_depth(
    source: &str,
    depth: usize,
    token_budget: &Rc<Cell<usize>>,
) -> Result<Vec<Stmt>, ParseError> {
    if depth >= MAX_PARSE_DEPTH {
        return Err(ParseError {
            message: format!("maximum parser nesting depth ({MAX_PARSE_DEPTH}) exceeded"),
        });
    }
    let remaining = token_budget.get();
    if remaining == 0 {
        return Err(ParseError { message: "page parse token budget exhausted".into() });
    }
    let limit = remaining.min(MAX_PARSE_TOKENS);
    let tokens = match tokenize_with_limit(source, limit) {
        Ok(tokens) => tokens,
        Err(error) => {
            // Charge the work performed before the failure. Using the source
            // length keeps a lexical error from draining the page allowance,
            // while a token-limit error is capped by the granted limit.
            let consumed = source.chars().count().saturating_add(1).min(limit);
            token_budget.set(token_budget.get().saturating_sub(consumed));
            let message = if limit < MAX_PARSE_TOKENS && error.message.contains("token count") {
                "page parse token budget exhausted".to_owned()
            } else {
                error.message
            };
            return Err(ParseError { message });
        }
    };
    token_budget.set(token_budget.get().saturating_sub(tokens.len()));
    let mut parser = Parser {
        tokens,
        position: 0,
        depth: Rc::new(Cell::new(depth)),
        token_budget: token_budget.clone(),
    };
    let mut statements = Vec::new();
    while parser.peek().kind != TokenKind::Eof {
        statements.push(parser.parse_statement()?);
    }
    Ok(statements)
}

impl Parser {
    /// Enters one recursive parse frame, rejecting nesting beyond the limit
    /// before any further recursive call is made.
    fn enter(&self) -> Result<DepthGuard, ParseError> {
        let next = self.depth.get().saturating_add(1);
        if next > MAX_PARSE_DEPTH {
            return Err(ParseError {
                message: format!("maximum parser nesting depth ({MAX_PARSE_DEPTH}) exceeded"),
            });
        }
        self.depth.set(next);
        Ok(DepthGuard(self.depth.clone()))
    }

    fn peek(&self) -> &Token {
        &self.tokens[self.position]
    }

    fn text(&self) -> &str {
        &self.peek().text
    }

    fn check(&self, text: &str) -> bool {
        let token = self.peek();
        if matches!(token.kind, TokenKind::String | TokenKind::Number | TokenKind::Template) {
            return false;
        }
        token.text == text
    }

    fn consume(&mut self, text: &str) -> bool {
        if self.check(text) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, text: &str) -> Result<(), ParseError> {
        if self.consume(text) {
            Ok(())
        } else {
            Err(ParseError { message: format!("expected '{text}', found '{}'", self.text()) })
        }
    }

    fn operator(&self) -> &str {
        if self.peek().kind == TokenKind::Operator { self.text() } else { "" }
    }

    fn expect_identifier(&mut self) -> Result<String, ParseError> {
        if self.peek().kind == TokenKind::Identifier {
            let name = self.text().to_owned();
            self.position += 1;
            Ok(name)
        } else {
            Err(ParseError { message: format!("expected identifier, found '{}'", self.text()) })
        }
    }

    fn parse_statement(&mut self) -> Result<Stmt, ParseError> {
        let _depth = self.enter()?;
        if self.consume(";") {
            return Ok(Stmt::Empty);
        }
        if self.check("{") {
            return self.parse_block().map(Stmt::Block);
        }
        match self.text() {
            "var" | "let" | "const" => self.parse_var(),
            "function" => self.parse_function_declaration(),
            "if" => self.parse_if(),
            "while" => self.parse_while(),
            "for" => self.parse_for(),
            "return" => {
                self.position += 1;
                let value = if self.check(";") { None } else { Some(self.parse_expression()?) };
                self.expect(";")?;
                Ok(Stmt::Return(value))
            }
            "break" => {
                self.position += 1;
                self.expect(";")?;
                Ok(Stmt::Break)
            }
            "continue" => {
                self.position += 1;
                self.expect(";")?;
                Ok(Stmt::Continue)
            }
            _ => {
                let expression = self.parse_expression()?;
                if !self.consume(";") && !self.check("}") && self.peek().kind != TokenKind::Eof {
                    return Err(ParseError {
                        message: format!("expected ';', found '{}'", self.text()),
                    });
                }
                Ok(Stmt::Expression(expression))
            }
        }
    }

    fn parse_var(&mut self) -> Result<Stmt, ParseError> {
        let kind = match self.text() {
            "var" => VarKind::Var,
            "let" => VarKind::Let,
            _ => VarKind::Const,
        };
        self.position += 1;
        let name = self.expect_identifier()?;
        let value = if self.consume("=") { Some(self.parse_expression()?) } else { None };
        self.expect(";")?;
        Ok(Stmt::Var { kind, name, value })
    }

    fn parse_function_declaration(&mut self) -> Result<Stmt, ParseError> {
        self.position += 1;
        let name = self.expect_identifier()?;
        let (params, body) = self.parse_function_tail()?;
        Ok(Stmt::Function { name, params, body })
    }

    fn parse_function_tail(&mut self) -> Result<(Vec<String>, Vec<Stmt>), ParseError> {
        self.expect("(")?;
        let mut params = Vec::new();
        if !self.check(")") {
            loop {
                params.push(self.expect_identifier()?);
                if !self.consume(",") {
                    break;
                }
            }
        }
        self.expect(")")?;
        let body = self.parse_block()?;
        Ok((params, body))
    }

    fn parse_block(&mut self) -> Result<Vec<Stmt>, ParseError> {
        let _depth = self.enter()?;
        self.expect("{")?;
        let mut statements = Vec::new();
        while !self.check("}") && self.peek().kind != TokenKind::Eof {
            statements.push(self.parse_statement()?);
        }
        self.expect("}")?;
        Ok(statements)
    }

    fn parse_if(&mut self) -> Result<Stmt, ParseError> {
        self.position += 1;
        self.expect("(")?;
        let test = self.parse_expression()?;
        self.expect(")")?;
        let then = Box::new(self.parse_statement()?);
        let otherwise =
            if self.consume("else") { Some(Box::new(self.parse_statement()?)) } else { None };
        Ok(Stmt::If { test, then, otherwise })
    }

    fn parse_while(&mut self) -> Result<Stmt, ParseError> {
        self.position += 1;
        self.expect("(")?;
        let test = self.parse_expression()?;
        self.expect(")")?;
        let body = Box::new(self.parse_statement()?);
        Ok(Stmt::While { test, body })
    }

    fn parse_for(&mut self) -> Result<Stmt, ParseError> {
        self.position += 1;
        self.expect("(")?;
        let init = if self.check(";") {
            None
        } else if matches!(self.text(), "var" | "let" | "const") {
            Some(Box::new(self.parse_var()?))
        } else {
            Some(Box::new(Stmt::Expression(self.parse_expression()?)))
        };
        let test = if self.consume(";") {
            None
        } else {
            let value = self.parse_expression()?;
            self.expect(";")?;
            Some(value)
        };
        let update = if self.check(")") { None } else { Some(self.parse_expression()?) };
        self.expect(")")?;
        let body = Box::new(self.parse_statement()?);
        Ok(Stmt::For { init, test, update, body })
    }

    fn parse_expression(&mut self) -> Result<Expr, ParseError> {
        let _depth = self.enter()?;
        self.parse_assignment()
    }

    fn parse_assignment(&mut self) -> Result<Expr, ParseError> {
        let _depth = self.enter()?;
        let target = self.parse_conditional()?;
        if self.consume("=") {
            let value = self.parse_assignment()?;
            if !matches!(target, Expr::Identifier(_) | Expr::Member { .. }) {
                return Err(ParseError { message: "invalid assignment target".into() });
            }
            return Ok(Expr::Assign { target: Box::new(target), value: Box::new(value) });
        }
        Ok(target)
    }

    fn parse_conditional(&mut self) -> Result<Expr, ParseError> {
        let test = self.parse_logical_or()?;
        if self.consume("?") {
            let then = self.parse_assignment()?;
            self.expect(":")?;
            let otherwise = self.parse_assignment()?;
            Ok(Expr::Conditional {
                test: Box::new(test),
                then: Box::new(then),
                otherwise: Box::new(otherwise),
            })
        } else {
            Ok(test)
        }
    }

    fn parse_logical_or(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.parse_logical_and()?;
        while self.consume("||") {
            let right = self.parse_logical_and()?;
            left =
                Expr::Logical { op: LogicalOp::Or, left: Box::new(left), right: Box::new(right) };
        }
        Ok(left)
    }

    fn parse_logical_and(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.parse_equality()?;
        while self.consume("&&") {
            let right = self.parse_equality()?;
            left =
                Expr::Logical { op: LogicalOp::And, left: Box::new(left), right: Box::new(right) };
        }
        Ok(left)
    }

    fn parse_equality(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.parse_relational()?;
        loop {
            let op = match self.operator() {
                "==" => BinaryOp::Equals,
                "!=" => BinaryOp::NotEquals,
                "===" => BinaryOp::StrictEquals,
                "!==" => BinaryOp::StrictNotEquals,
                _ => break,
            };
            self.position += 1;
            let right = self.parse_relational()?;
            left = Expr::Binary { op, left: Box::new(left), right: Box::new(right) };
        }
        Ok(left)
    }

    fn parse_relational(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.parse_additive()?;
        loop {
            let op = match self.operator() {
                "<" => BinaryOp::Less,
                "<=" => BinaryOp::LessOrEqual,
                ">" => BinaryOp::Greater,
                ">=" => BinaryOp::GreaterOrEqual,
                _ => break,
            };
            self.position += 1;
            let right = self.parse_additive()?;
            left = Expr::Binary { op, left: Box::new(left), right: Box::new(right) };
        }
        Ok(left)
    }

    fn parse_additive(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.parse_multiplicative()?;
        loop {
            let op = match self.operator() {
                "+" => BinaryOp::Add,
                "-" => BinaryOp::Subtract,
                _ => break,
            };
            self.position += 1;
            let right = self.parse_multiplicative()?;
            left = Expr::Binary { op, left: Box::new(left), right: Box::new(right) };
        }
        Ok(left)
    }

    fn parse_multiplicative(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.operator() {
                "*" => BinaryOp::Multiply,
                "/" => BinaryOp::Divide,
                "%" => BinaryOp::Modulo,
                _ => break,
            };
            self.position += 1;
            let right = self.parse_unary()?;
            left = Expr::Binary { op, left: Box::new(left), right: Box::new(right) };
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr, ParseError> {
        let _depth = self.enter()?;
        let op = match self.operator() {
            "-" => Some(UnaryOp::Negate),
            "!" => Some(UnaryOp::Not),
            "typeof" => Some(UnaryOp::TypeOf),
            _ => None,
        };
        if let Some(op) = op {
            self.position += 1;
            return Ok(Expr::Unary { op, value: Box::new(self.parse_unary()?) });
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> Result<Expr, ParseError> {
        let mut expression = self.parse_primary()?;
        loop {
            if self.consume("(") {
                let mut args = Vec::new();
                if !self.check(")") {
                    loop {
                        args.push(self.parse_assignment()?);
                        if !self.consume(",") {
                            break;
                        }
                    }
                }
                self.expect(")")?;
                expression = Expr::Call { callee: Box::new(expression), args };
            } else if self.consume(".") {
                let property = self.expect_identifier()?;
                expression = Expr::Member {
                    object: Box::new(expression),
                    property: Box::new(Expr::String(property)),
                };
            } else if self.consume("[") {
                let property = self.parse_expression()?;
                self.expect("]")?;
                expression =
                    Expr::Member { object: Box::new(expression), property: Box::new(property) };
            } else {
                break;
            }
        }
        Ok(expression)
    }

    fn arrow_params_ahead(&self) -> bool {
        if !self.check("(") {
            return false;
        }
        let mut depth = 0usize;
        let mut index = self.position;
        while index < self.tokens.len() {
            match self.tokens[index].text.as_str() {
                "(" => depth += 1,
                ")" => {
                    depth -= 1;
                    if depth == 0 {
                        return self.tokens.get(index + 1).is_some_and(|token| token.text == "=>");
                    }
                }
                _ => {}
            }
            index += 1;
        }
        false
    }

    fn parse_arrow(&mut self, params: Vec<String>) -> Result<Expr, ParseError> {
        self.expect("=>")?;
        let body = if self.check("{") {
            self.parse_block()?
        } else {
            vec![Stmt::Return(Some(self.parse_assignment()?))]
        };
        Ok(Expr::Function { params, body })
    }

    fn parse_primary(&mut self) -> Result<Expr, ParseError> {
        let _depth = self.enter()?;
        if self.peek().kind == TokenKind::Identifier
            && self.tokens.get(self.position + 1).is_some_and(|token| token.text == "=>")
        {
            let name = self.text().to_owned();
            self.position += 1;
            return self.parse_arrow(vec![name]);
        }
        if self.arrow_params_ahead() {
            self.expect("(")?;
            let mut params = Vec::new();
            if !self.check(")") {
                loop {
                    params.push(self.expect_identifier()?);
                    if !self.consume(",") {
                        break;
                    }
                }
            }
            self.expect(")")?;
            return self.parse_arrow(params);
        }
        let token = self.peek().clone();
        match token.kind {
            TokenKind::Number => {
                self.position += 1;
                let value = token.text.parse::<f64>().map_err(|_| ParseError {
                    message: format!("invalid number '{}'", token.text),
                })?;
                Ok(Expr::Number(value))
            }
            TokenKind::String => {
                self.position += 1;
                Ok(Expr::String(token.text))
            }
            TokenKind::Template => {
                self.position += 1;
                parse_template(&token.text, self.depth.get(), &self.token_budget)
            }
            TokenKind::Identifier => {
                self.position += 1;
                Ok(Expr::Identifier(token.text))
            }
            TokenKind::Keyword => match token.text.as_str() {
                "true" => {
                    self.position += 1;
                    Ok(Expr::Bool(true))
                }
                "false" => {
                    self.position += 1;
                    Ok(Expr::Bool(false))
                }
                "null" => {
                    self.position += 1;
                    Ok(Expr::Null)
                }
                "undefined" => {
                    self.position += 1;
                    Ok(Expr::Undefined)
                }
                "function" => {
                    self.position += 1;
                    let (params, body) = self.parse_function_tail()?;
                    Ok(Expr::Function { params, body })
                }
                other => Err(ParseError { message: format!("unexpected keyword '{other}'") }),
            },
            TokenKind::Punctuation => match token.text.as_str() {
                "(" => {
                    self.position += 1;
                    let value = self.parse_expression()?;
                    self.expect(")")?;
                    Ok(value)
                }
                "[" => {
                    self.position += 1;
                    let mut values = Vec::new();
                    if !self.check("]") {
                        loop {
                            values.push(self.parse_assignment()?);
                            if !self.consume(",") {
                                break;
                            }
                        }
                    }
                    self.expect("]")?;
                    Ok(Expr::Array(values))
                }
                "{" => {
                    self.position += 1;
                    let mut properties = Vec::new();
                    if !self.check("}") {
                        loop {
                            let name = match self.peek().kind {
                                TokenKind::Identifier | TokenKind::String | TokenKind::Keyword => {
                                    self.text().to_owned()
                                }
                                _ => {
                                    return Err(ParseError {
                                        message: format!("invalid property name '{}'", self.text()),
                                    });
                                }
                            };
                            self.position += 1;
                            self.expect(":")?;
                            let value = self.parse_assignment()?;
                            properties.push((name, value));
                            if !self.consume(",") {
                                break;
                            }
                        }
                    }
                    self.expect("}")?;
                    Ok(Expr::Object(properties))
                }
                other => Err(ParseError { message: format!("unexpected token '{other}'") }),
            },
            _ => Err(ParseError { message: format!("unexpected token '{}'", token.text) }),
        }
    }
}

fn parse_template(
    source: &str,
    depth: usize,
    token_budget: &Rc<Cell<usize>>,
) -> Result<Expr, ParseError> {
    let mut parts = Vec::new();
    let mut rest = source;
    while let Some(start) = rest.find("${") {
        if start > 0 {
            parts.push(Expr::String(rest[..start].to_owned()));
        }
        let after = &rest[start + 2..];
        let mut brace_depth = 1usize;
        let mut end = None;
        let mut quote = None;
        for (index, character) in after.char_indices() {
            if let Some(current) = quote {
                if character == current {
                    quote = None;
                }
                continue;
            }
            match character {
                '\'' | '"' | '`' => quote = Some(character),
                '{' => brace_depth += 1,
                '}' => {
                    brace_depth -= 1;
                    if brace_depth == 0 {
                        end = Some(index);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = end else {
            return Err(ParseError { message: "unterminated template interpolation".into() });
        };
        let expression = parse_with_depth(&after[..end], depth + 1, token_budget)?;
        let expression = match expression.first() {
            Some(Stmt::Expression(expression)) => expression.clone(),
            None => Expr::Undefined,
            _ => return Err(ParseError { message: "invalid template interpolation".into() }),
        };
        parts.push(expression);
        rest = &after[end + 1..];
    }
    if !rest.is_empty() {
        parts.push(Expr::String(rest.to_owned()));
    }
    Ok(Expr::Template(parts))
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn deeply_nested_source_is_rejected_with_a_clean_error() {
        let cases: Vec<(&str, String)> = vec![
            ("parentheses", format!("{}1{}", "(".repeat(5_000), ")".repeat(5_000))),
            ("unary chain", format!("{}1", "!".repeat(5_000))),
            ("assignment chain", format!("{}=1", "a=".repeat(5_000))),
            ("nested blocks", format!("{}{}{}", "{".repeat(5_000), ";", "}".repeat(5_000))),
        ];
        for (name, source) in cases {
            let error = parse(&source).expect_err(name);
            assert!(
                error.message.contains("nesting depth"),
                "{name} was not rejected by the depth guard: {}",
                error.message
            );
        }
    }

    #[test]
    fn parses_expressions_and_control_flow() {
        let program =
            parse("let x = 2; function add(a, b) { return a + b; } if (x < 3) { x = add(x, 4); }")
                .expect("program");
        assert_eq!(program.len(), 3);
    }
}
