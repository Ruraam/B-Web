//! Evaluator for the CSS `calc()` length subset.
//!
//! Supports `+`, `-`, `*`, `/`, parentheses and mixed units (`px`, `em`, `rem`,
//! `%`) resolved to logical pixels at layout time. Bare numbers are treated as
//! pixels. Anything unparseable or non-finite resolves to `None`.

/// A token in a `calc()` expression.
#[derive(Debug, Clone, PartialEq)]
enum Token {
    Number(f32, Unit),
    Plus,
    Minus,
    Star,
    Slash,
    Open,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Unit {
    Px,
    Em,
    Rem,
    Percent,
    Vw,
    Vh,
    Number,
}

/// Parses a numeric literal with an optional unit.
fn number_token(chars: &[char], index: &mut usize) -> Option<Token> {
    let start = *index;
    while *index < chars.len() && (chars[*index].is_ascii_digit() || chars[*index] == '.') {
        *index += 1;
    }
    if *index == start {
        return None;
    }
    let number: f32 = chars[start..*index].iter().collect::<String>().parse().ok()?;
    if !number.is_finite() {
        return None;
    }
    let unit_start = *index;
    while *index < chars.len() && (chars[*index].is_ascii_alphabetic() || chars[*index] == '%') {
        *index += 1;
    }
    let unit_text: String = chars[unit_start..*index].iter().collect();
    let unit = match unit_text.to_ascii_lowercase().as_str() {
        "" => Unit::Number,
        "px" => Unit::Px,
        "em" => Unit::Em,
        "rem" => Unit::Rem,
        "%" => Unit::Percent,
        "vw" => Unit::Vw,
        "vh" => Unit::Vh,
        _ => return None,
    };
    Some(Token::Number(number, unit))
}

/// Tokenizes a `calc()` expression body.
fn tokenize(chars: &[char]) -> Option<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            c if c.is_ascii_whitespace() => index += 1,
            '+' => {
                tokens.push(Token::Plus);
                index += 1;
            }
            '-' => {
                tokens.push(Token::Minus);
                index += 1;
            }
            '*' => {
                tokens.push(Token::Star);
                index += 1;
            }
            '/' => {
                tokens.push(Token::Slash);
                index += 1;
            }
            '(' => {
                tokens.push(Token::Open);
                index += 1;
            }
            ')' => {
                tokens.push(Token::Close);
                index += 1;
            }
            c if c.is_ascii_digit() || c == '.' => {
                tokens.push(number_token(chars, &mut index)?);
            }
            _ => return None,
        }
    }
    Some(tokens)
}

/// A resolved value: pixels plus whether it came from a bare number. CSS
/// arithmetic requires at most one side of `*` to carry a unit; we keep the
/// flag to reject `px * px` while still allowing `2 * 10px`.
#[derive(Debug, Clone, Copy)]
struct Value {
    px: f32,
    bare: bool,
}

struct Parser<'a> {
    tokens: &'a [Token],
    index: usize,
    font_size: f32,
    basis: Option<f32>,
    viewport: (f32, f32),
}

impl<'a> Parser<'a> {
    fn resolve_unit(&self, number: f32, unit: Unit) -> Option<Value> {
        let px = match unit {
            Unit::Px => number,
            Unit::Em => number * self.font_size,
            Unit::Rem => number * 16.0,
            Unit::Percent => number * self.basis? / 100.0,
            Unit::Vw => number * self.viewport.0 / 100.0,
            Unit::Vh => number * self.viewport.1 / 100.0,
            Unit::Number => number,
        };
        px.is_finite().then_some(Value { px, bare: unit == Unit::Number })
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.index)
    }

    fn expression(&mut self) -> Option<Value> {
        let mut value = self.term()?;
        loop {
            match self.peek() {
                Some(Token::Plus) => {
                    self.index += 1;
                    let rhs = self.term()?;
                    value = Value { px: value.px + rhs.px, bare: false };
                }
                Some(Token::Minus) => {
                    self.index += 1;
                    let rhs = self.term()?;
                    value = Value { px: value.px - rhs.px, bare: false };
                }
                _ => break,
            }
        }
        value.px.is_finite().then_some(value)
    }

    fn term(&mut self) -> Option<Value> {
        let mut value = self.factor()?;
        loop {
            match self.peek() {
                Some(Token::Star) => {
                    self.index += 1;
                    let rhs = self.factor()?;
                    if !value.bare && !rhs.bare {
                        return None;
                    }
                    value = Value { px: value.px * rhs.px, bare: value.bare && rhs.bare };
                }
                Some(Token::Slash) => {
                    self.index += 1;
                    let rhs = self.factor()?;
                    if !rhs.bare || rhs.px == 0.0 {
                        return None;
                    }
                    value = Value { px: value.px / rhs.px, bare: value.bare };
                }
                _ => break,
            }
        }
        value.px.is_finite().then_some(value)
    }

    fn factor(&mut self) -> Option<Value> {
        match self.peek()? {
            Token::Minus => {
                self.index += 1;
                let value = self.factor()?;
                Some(Value { px: -value.px, bare: value.bare })
            }
            Token::Plus => {
                self.index += 1;
                self.factor()
            }
            Token::Open => {
                self.index += 1;
                let value = self.expression()?;
                if self.peek() != Some(&Token::Close) {
                    return None;
                }
                self.index += 1;
                Some(value)
            }
            Token::Number(number, unit) => {
                let number = *number;
                let unit = *unit;
                self.index += 1;
                self.resolve_unit(number, unit)
            }
            _ => None,
        }
    }
}

/// Evaluates a `calc(...)` expression body to logical pixels.
#[must_use]
pub fn eval_calc(
    body: &str,
    font_size: f32,
    basis: Option<f32>,
    viewport: (f32, f32),
) -> Option<f32> {
    let chars: Vec<char> = body.chars().collect();
    let tokens = tokenize(&chars)?;
    if tokens.is_empty() {
        return None;
    }
    let mut parser = Parser { tokens: &tokens, index: 0, font_size, basis, viewport };
    let value = parser.expression()?;
    if parser.index != tokens.len() {
        return None;
    }
    value.px.is_finite().then_some(value.px)
}

/// Extracts the body of `calc(...)`, returning the inner text.
#[must_use]
pub fn calc_body(value: &str) -> Option<&str> {
    let value = value.trim();
    let lower = value.to_ascii_lowercase();
    if !lower.starts_with("calc(") || !value.ends_with(')') {
        return None;
    }
    Some(&value[5..value.len() - 1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_arithmetic() {
        assert_eq!(eval_calc("100px - 20px", 16.0, None, (1280.0, 720.0)), Some(80.0));
        assert_eq!(eval_calc("100px-20px", 16.0, None, (1280.0, 720.0)), Some(80.0));
        assert_eq!(eval_calc("2 * 10px + 5px", 16.0, None, (1280.0, 720.0)), Some(25.0));
    }

    #[test]
    fn mixed_units_with_basis() {
        assert_eq!(eval_calc("100% - 20px", 16.0, Some(200.0), (1280.0, 720.0)), Some(180.0));
        assert_eq!(eval_calc("50% / 2", 16.0, Some(200.0), (1280.0, 720.0)), Some(50.0));
    }

    #[test]
    fn em_rem_and_parens() {
        assert_eq!(eval_calc("(1em + 1rem) * 2", 16.0, None, (1280.0, 720.0)), Some(64.0));
    }

    #[test]
    fn viewport_units() {
        assert_eq!(eval_calc("50vw / 2", 16.0, None, (1280.0, 720.0)), Some(320.0));
        assert_eq!(eval_calc("100vh - 20px", 16.0, None, (1280.0, 720.0)), Some(700.0));
    }

    #[test]
    fn rejects_invalid_arithmetic() {
        assert_eq!(eval_calc("10px * 20px", 16.0, None, (1280.0, 720.0)), None);
        assert_eq!(eval_calc("10px / 0", 16.0, None, (1280.0, 720.0)), None);
        assert_eq!(eval_calc("50% - 10px", 16.0, None, (1280.0, 720.0)), None); // no basis
    }

    #[test]
    fn calc_body_extraction() {
        assert_eq!(calc_body("calc(100% - 20px)"), Some("100% - 20px"));
        assert_eq!(calc_body("100px"), None);
    }
}
