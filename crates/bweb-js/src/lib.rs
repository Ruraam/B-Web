//! B-Web's JavaScript runtime, implemented in Rust.
//!
//! No external JavaScript engine is embedded. The crate provides a lexer, a
//! parser and a tree-walking evaluator for the language subset used by B-Web's
//! scripts, plus a host bridge for DOM interaction.

mod ast;
mod lexer;
mod parser;
mod runtime;

pub use ast::{BinaryOp, Expr, LogicalOp, Stmt, UnaryOp, VarKind};
pub use lexer::{LexError, Token, TokenKind, tokenize};
pub use parser::{ParseError, parse};
pub use runtime::{Env, Host, JsError, NativeFn, Object, Runtime, Value};

/// Explicit status of the JavaScript runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeStatus {
    /// The runtime is available to the shell.
    Ready,
}

#[cfg(test)]
mod tests {
    use super::{Runtime, Value};

    #[test]
    fn evaluates_arithmetic_and_variables() {
        let mut runtime = Runtime::new();
        assert_eq!(runtime.eval("1 + 2 * 3").unwrap().display(), "7");
        assert_eq!(Runtime::new().eval("let x = 4; x * x").unwrap().display(), "16");
    }

    #[test]
    fn evaluates_functions_and_closures() {
        let value = Runtime::new()
            .eval("function make(n) { return function(x) { return x + n; }; } let add = make(5); add(3)")
            .unwrap();
        assert_eq!(value.display(), "8");
    }

    #[test]
    fn evaluates_control_flow_and_loops() {
        let value = Runtime::new()
            .eval("let total = 0; for (let i = 0; i < 5; i = i + 1) { total = total + i; } total")
            .unwrap();
        assert_eq!(value.display(), "10");
        let value = Runtime::new().eval("let n = 0; while (n < 3) { n = n + 1; } n").unwrap();
        assert_eq!(value.display(), "3");
    }

    #[test]
    fn evaluates_objects_arrays_and_strings() {
        let value = Runtime::new().eval("let a = [1, 2, 3]; let o = {x: 4}; a[1] + o.x").unwrap();
        assert_eq!(value.display(), "6");
        let value = Runtime::new().eval("\"ab\" + 3").unwrap_or(Value::Undefined);
        assert_eq!(value.display(), "ab3");
    }

    #[test]
    fn evaluates_arrow_functions_and_template_literals() {
        let value = Runtime::new()
            .eval("const f = x => x * 2; const g = (a, b) => a + b; const name = 'JS'; `hello ${name}: ${f(3)} ${g(1, 2)}`")
            .unwrap();
        assert_eq!(value.display(), "hello JS: 6 3");
    }

    #[test]
    fn evaluates_builtins_and_string_methods() {
        let value = Runtime::new()
            .eval("Math.max(2, 7) + Math.floor(3.9) + parseInt('42px') + 'ab'.toUpperCase().length")
            .unwrap();
        assert_eq!(value.display(), "54");
        let value = Runtime::new().eval("'  Hi  '.trim().toLowerCase()").unwrap();
        assert_eq!(value.display(), "hi");
    }

    #[test]
    fn evaluates_array_methods() {
        let value = Runtime::new().eval("let a = [1, 2, 3]; a.push(4); a.join('-')").unwrap();
        assert_eq!(value.display(), "1-2-3-4");
        let value = Runtime::new()
            .eval("let total = 0; [1, 2, 3].forEach(x => { total = total + x; }); total")
            .unwrap();
        assert_eq!(value.display(), "6");
    }

    #[test]
    fn captures_print_output() {
        let mut runtime = Runtime::new();
        runtime.eval("print(\"hi\"); console.log(2);").expect("script");
        assert_eq!(runtime.output(), ["hi", "2"]);
    }

    #[test]
    fn reports_undefined_identifiers() {
        let error = Runtime::new().eval("missing").unwrap_err();
        assert!(error.message.contains("not defined"));
    }
}
