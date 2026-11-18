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
pub use runtime::{Env, Host, JsError, NativeFn, Object, PAGE_INSTRUCTION_BUDGET, Runtime, Value};

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

    #[test]
    fn global_instruction_budget_stops_nested_loops() {
        // One loop of 100k is allowed by the per-loop guard, but nested loops
        // multiply the work; the global budget must abort them.
        let error = Runtime::new()
            .eval("let n = 0; for (let i = 0; i < 100000; i = i + 1) { for (let j = 0; j < 100000; j = j + 1) { n = n + 1; } }")
            .unwrap_err();
        assert!(
            error.message.contains("budget") || error.message.contains("iteration"),
            "unexpected error: {}",
            error.message
        );
    }

    #[test]
    fn recursion_depth_is_limited() {
        let error = Runtime::new().eval("function f(n) { return f(n + 1); } f(0)").unwrap_err();
        assert!(error.message.contains("call depth"), "unexpected error: {}", error.message);
    }

    #[test]
    fn event_budget_stops_a_long_handler_and_the_runtime_recovers() {
        let mut runtime = Runtime::new();
        runtime
            .eval("function spin() { while (true) { } } function ok() { return 7; }")
            .expect("define functions");
        let spin = runtime.eval("spin").expect("spin function");
        let error = runtime.call_event(spin, vec![]).expect_err("handler must be stopped");
        assert!(error.message.contains("budget"), "unexpected error: {}", error.message);

        // A failed handler leaves the runtime usable: fresh eval and a second
        // handler both still work.
        assert_eq!(runtime.eval("1 + 1").expect("eval after failure").display(), "2");
        let ok = runtime.eval("ok").expect("ok function");
        assert_eq!(runtime.call_event(ok, vec![]).expect("second handler").display(), "7");
    }

    #[test]
    fn event_budget_keeps_the_call_depth_limit_and_restores_it() {
        let mut runtime = Runtime::new();
        runtime
            .eval("function recurse() { return recurse(); } function down(n) { if (n == 0) { return 0; } return down(n - 1); }")
            .expect("define functions");
        let recurse = runtime.eval("recurse").expect("recurse function");
        let error = runtime.call_event(recurse, vec![]).expect_err("recursion must be stopped");
        assert!(error.message.contains("call depth"), "unexpected error: {}", error.message);

        // The depth counter was unwound by the error, so a bounded recursion
        // can still run to completion.
        let down = runtime.eval("down").expect("down function");
        let value = runtime.call_event(down, vec![Value::Number(5.0)]).expect("bounded recursion");
        assert_eq!(value.display(), "0");
    }

    #[test]
    fn event_handlers_use_a_smaller_budget_than_page_scripts() {
        let mut runtime = Runtime::new();
        runtime
            .eval("function work() { let s = 0; for (let i = 0; i < 50000; i = i + 1) { s = s + 1; } return s; }")
            .expect("define work");
        let work = runtime.eval("work").expect("work function");
        assert!(
            runtime.call_event(work.clone(), vec![]).is_err(),
            "work exceeds the interactive budget"
        );
        assert_eq!(runtime.call(work, vec![]).expect("full budget").display(), "50000");
    }

    #[test]
    fn page_budget_is_shared_between_evaluations() {
        let mut runtime = Runtime::new();
        runtime.set_page_budget(100);
        let mut failed = None;
        for _ in 0..200 {
            if let Err(error) = runtime.eval("1 + 1") {
                failed = Some(error.message);
                break;
            }
        }
        let message = failed.expect("the shared budget must eventually be exhausted");
        assert!(message.contains("page instruction budget"), "unexpected error: {message}");
        assert_eq!(runtime.page_budget_remaining(), Some(0));

        // Clearing the page budget restores independent per-evaluation budgets.
        runtime.clear_page_budget();
        assert_eq!(runtime.page_budget_remaining(), None);
        assert_eq!(runtime.eval("1 + 1").expect("eval").display(), "2");
    }

    #[test]
    fn page_budget_caps_event_callbacks() {
        let mut runtime = Runtime::new();
        runtime.eval("function spin() { while (true) { } }").expect("define handler");
        let spin = runtime.eval("spin").expect("handler");
        runtime.set_page_budget(500);
        let error =
            runtime.call_event(spin, vec![]).expect_err("page budget must stop the handler");
        assert!(error.message.contains("budget"), "unexpected error: {}", error.message);
        assert_eq!(runtime.page_budget_remaining(), Some(0));
    }

    #[test]
    fn oversized_scripts_are_rejected_during_parsing() {
        let source = format!("let x = {};", "1+".repeat(200_001));
        let error = Runtime::new().eval(&source).expect_err("token limit");
        assert!(error.message.contains("token count"), "unexpected error: {}", error.message);
    }

    #[test]
    fn budget_is_reset_between_evaluations() {
        let mut runtime = Runtime::new();
        // A moderately long but bounded loop should succeed.
        let value = runtime
            .eval("let s = 0; for (let i = 0; i < 1000; i = i + 1) { s = s + 1; } s")
            .expect("script");
        assert_eq!(value.display(), "1000");
        // A second evaluation still has a full budget.
        let value = runtime.eval("1 + 1").expect("script");
        assert_eq!(value.display(), "2");
    }
}
