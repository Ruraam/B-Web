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
pub use runtime::{
    Env, Host, JsError, MAX_ARRAY_LENGTH, MAX_OBJECT_PROPERTIES, MAX_STRING_BYTES, NativeFn,
    Object, PAGE_INSTRUCTION_BUDGET, PAGE_MEMORY_BUDGET, PAGE_PARSE_TOKEN_BUDGET, Runtime, Value,
};

/// Explicit status of the JavaScript runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeStatus {
    /// The runtime is available to the shell.
    Ready,
}

#[cfg(test)]
mod tests {
    use super::{
        Host, JsError, Object, PAGE_INSTRUCTION_BUDGET, PAGE_MEMORY_BUDGET, Runtime, Value,
    };
    use std::{cell::Cell, cell::RefCell, rc::Rc};

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
    fn page_parse_budget_is_cumulative_across_scripts() {
        let mut runtime = Runtime::new();
        runtime.set_page_budget(PAGE_INSTRUCTION_BUDGET);
        runtime.set_page_parse_budget(40);
        // "let a = 1;" produces six tokens including end-of-file, so six
        // scripts consume 36 of the 40 tokens.
        for _ in 0..6 {
            runtime.eval("let a = 1;").expect("script within the parse budget");
        }
        assert_eq!(runtime.page_parse_budget_remaining(), Some(4));

        let error = runtime.eval("let b = 2;").expect_err("parse budget must stop the script");
        assert!(
            error.message.contains("page parse token budget"),
            "unexpected error: {}",
            error.message
        );
        assert_eq!(runtime.page_parse_budget_remaining(), Some(0));
    }

    #[test]
    fn exhausted_page_budget_skips_parsing_entirely() {
        let mut runtime = Runtime::new();
        runtime.set_page_budget(0);
        // The source is malformed; reaching the parser would report a syntax
        // error instead of the page budget error.
        let error = runtime.eval("let = ;").expect_err("no parsing without a budget");
        assert!(
            error.message.contains("page instruction budget"),
            "unexpected error: {}",
            error.message
        );
    }

    #[test]
    fn failed_parses_still_charge_the_page_parse_budget() {
        let mut runtime = Runtime::new();
        runtime.set_page_budget(PAGE_INSTRUCTION_BUDGET);
        runtime.set_page_parse_budget(100);
        let before = runtime.page_parse_budget_remaining().expect("parse budget");
        runtime.eval("'unterminated").expect_err("lexical error");
        let after = runtime.page_parse_budget_remaining().expect("parse budget");
        assert!(after < before, "failed parsing must still be charged");
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

    #[test]
    fn memory_budget_is_active_by_default() {
        let runtime = Runtime::new();
        assert_eq!(runtime.memory_budget(), PAGE_MEMORY_BUDGET);
        assert_eq!(runtime.memory_used(), 0);
    }

    #[test]
    fn string_concatenation_is_bounded_by_the_memory_budget() {
        let mut runtime = Runtime::new();
        runtime.set_memory_budget(4096);
        let error = runtime
            .eval("let s = 'x'; while (true) { s = s + 'x'; }")
            .expect_err("concatenation must exhaust the memory budget");
        assert!(error.message.contains("memory budget"), "unexpected error: {}", error.message);
        assert!(runtime.memory_used() <= 4096);
    }

    #[test]
    fn oversized_strings_are_rejected() {
        let mut runtime = Runtime::new();
        runtime.set_allocation_limits(32, 100, 100);
        let error =
            runtime.eval("'0123456789012345678901234567890123'").expect_err("string too large");
        assert!(error.message.contains("string length"), "unexpected error: {}", error.message);
    }

    #[test]
    fn initial_ast_is_charged_to_the_memory_budget() {
        let mut runtime = Runtime::new();
        runtime.set_memory_budget(2000);
        runtime.set_memory_used(2000);
        // Parsing itself allocates the AST, which must be charged before the
        // script is allowed to run.
        let error = runtime.eval("1 + 1").expect_err("AST allocation must fail");
        assert!(error.message.contains("memory budget"), "unexpected error: {}", error.message);
        assert_eq!(runtime.memory_used(), 2000);

        // Running an already parsed program needs no runtime allocation for
        // scalar arithmetic, so it still works at the boundary.
        let statements = super::parse("1 + 1").expect("parse");
        assert_eq!(runtime.run(&statements).expect("scalars").display(), "2");
    }

    #[test]
    fn array_length_is_limited() {
        let mut runtime = Runtime::new();
        runtime.set_allocation_limits(1_000_000, 3, 100);
        let error = runtime
            .eval("let a = []; a.push(1); a.push(2); a.push(3); a.push(4);")
            .expect_err("array must not grow past the limit");
        assert!(error.message.contains("array length"), "unexpected error: {}", error.message);

        let mut runtime = Runtime::new();
        runtime.set_allocation_limits(1_000_000, 3, 100);
        let error = runtime.eval("[1, 2, 3, 4]").expect_err("literal array too large");
        assert!(error.message.contains("array length"), "unexpected error: {}", error.message);
    }

    #[test]
    fn array_length_assignment_is_bounded() {
        let mut runtime = Runtime::new();
        runtime.set_allocation_limits(1_000_000, 5, 100);
        let error = runtime.eval("let a = []; a.length = 1000000;").expect_err("length too large");
        assert!(error.message.contains("array length"), "unexpected error: {}", error.message);
    }

    #[test]
    fn join_output_is_bounded() {
        let mut runtime = Runtime::new();
        runtime.set_allocation_limits(8, 100, 100);
        let error =
            runtime.eval("['abcde', 'fghij'].join('')").expect_err("joined string too large");
        assert!(error.message.contains("string length"), "unexpected error: {}", error.message);
    }

    #[test]
    fn object_property_count_is_limited() {
        let mut runtime = Runtime::new();
        runtime.set_allocation_limits(1_000_000, 100, 3);
        let error =
            runtime.eval("let o = {a: 1, b: 2, c: 3, d: 4};").expect_err("object too large");
        assert!(error.message.contains("object property"), "unexpected error: {}", error.message);

        let mut runtime = Runtime::new();
        runtime.set_allocation_limits(1_000_000, 100, 3);
        let error = runtime
            .eval("let o = {}; o.a = 1; o.b = 2; o.c = 3; o.d = 4;")
            .expect_err("assignment must enforce the limit");
        assert!(error.message.contains("object property"), "unexpected error: {}", error.message);
    }

    #[test]
    fn retained_print_output_is_bounded() {
        let mut runtime = Runtime::new();
        runtime.set_memory_budget(2048);
        let error = runtime
            .eval("while (true) { print('0123456789'); }")
            .expect_err("print output must exhaust the memory budget");
        assert!(error.message.contains("memory budget"), "unexpected error: {}", error.message);
        let retained: usize = runtime.output().iter().map(String::len).sum();
        assert!(retained <= 2048, "retained output exceeds the budget: {retained}");
    }

    struct LargeStringHost;

    impl Host for LargeStringHost {
        fn get(&self, _object: &str, _property: &str) -> Value {
            Value::String("x".repeat(128))
        }
    }

    #[test]
    fn host_returned_strings_are_charged() {
        let mut runtime = Runtime::new();
        runtime.set_allocation_limits(64, 100, 100);
        runtime.set_host(Box::new(LargeStringHost));
        runtime.define_global("host", Value::Host("host".into()));
        let error = runtime.eval("host.value").expect_err("host string too large");
        assert!(error.message.contains("string length"), "unexpected error: {}", error.message);
    }

    struct HostArray;

    impl Host for HostArray {
        fn get(&self, _object: &str, _property: &str) -> Value {
            let object = Rc::new(RefCell::new(Object { is_array: true, ..Object::default() }));
            object.borrow_mut().properties.insert("length".into(), Value::Number(10.0));
            Value::Object(object)
        }
    }

    struct HostObject;

    impl Host for HostObject {
        fn get(&self, _object: &str, _property: &str) -> Value {
            let object = Rc::new(RefCell::new(Object::default()));
            for name in ["a", "b", "c", "d"] {
                object.borrow_mut().properties.insert(name.into(), Value::Number(1.0));
            }
            Value::Object(object)
        }
    }

    #[test]
    fn host_arrays_and_objects_respect_the_limits() {
        let mut runtime = Runtime::new();
        runtime.set_allocation_limits(1_000_000, 3, 100);
        runtime.set_host(Box::new(HostArray));
        runtime.define_global("host", Value::Host("host".into()));
        let error = runtime.eval("host.items").expect_err("host array too long");
        assert!(error.message.contains("array length"), "unexpected error: {}", error.message);

        let mut runtime = Runtime::new();
        runtime.set_allocation_limits(1_000_000, 100, 3);
        runtime.set_host(Box::new(HostObject));
        runtime.define_global("host", Value::Host("host".into()));
        let error = runtime.eval("host.data").expect_err("host object too large");
        assert!(error.message.contains("object property"), "unexpected error: {}", error.message);
    }

    struct NestedHost;

    impl Host for NestedHost {
        fn get(&self, _object: &str, _property: &str) -> Value {
            let inner = Rc::new(RefCell::new(Object::default()));
            inner.borrow_mut().properties.insert("data".into(), Value::String("x".repeat(100)));
            let outer = Rc::new(RefCell::new(Object::default()));
            outer.borrow_mut().properties.insert("inner".into(), Value::Object(inner));
            Value::Object(outer)
        }
    }

    struct CyclicHost;

    impl Host for CyclicHost {
        fn get(&self, _object: &str, _property: &str) -> Value {
            let object = Rc::new(RefCell::new(Object::default()));
            object.borrow_mut().properties.insert("self".into(), Value::Object(object.clone()));
            Value::Object(object)
        }
    }

    struct SharedHost;

    impl Host for SharedHost {
        fn get(&self, _object: &str, _property: &str) -> Value {
            let shared = Rc::new(RefCell::new(Object::default()));
            shared.borrow_mut().properties.insert("data".into(), Value::String("x".repeat(100)));
            let outer = Rc::new(RefCell::new(Object::default()));
            outer.borrow_mut().properties.insert("a".into(), Value::Object(shared.clone()));
            outer.borrow_mut().properties.insert("b".into(), Value::Object(shared));
            Value::Object(outer)
        }
    }

    struct DeepHost;

    impl Host for DeepHost {
        fn get(&self, _object: &str, _property: &str) -> Value {
            let mut current = Rc::new(RefCell::new(Object::default()));
            current.borrow_mut().properties.insert("value".into(), Value::Number(1.0));
            for _ in 0..20 {
                let parent = Rc::new(RefCell::new(Object::default()));
                parent.borrow_mut().properties.insert("child".into(), Value::Object(current));
                current = parent;
            }
            Value::Object(current)
        }
    }

    #[test]
    fn deeply_nested_host_objects_are_rejected() {
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(DeepHost));
        runtime.define_global("host", Value::Host("host".into()));
        let error = runtime.eval("host.node").expect_err("nesting limit");
        assert!(error.message.contains("nesting"), "unexpected error: {}", error.message);
    }

    struct BudgetHost {
        materialized: Rc<Cell<usize>>,
    }

    impl BudgetHost {
        fn note(&self) {
            self.materialized.set(self.materialized.get() + 1);
        }
    }

    impl Host for BudgetHost {
        fn call(
            &mut self,
            _object: &str,
            _method: &str,
            _args: &[Value],
        ) -> Result<Value, JsError> {
            self.note();
            Ok(Value::String("x".repeat(100)))
        }

        fn call_with_budget(
            &mut self,
            _object: &str,
            _method: &str,
            _args: &[Value],
            reserve: &mut dyn FnMut(usize) -> bool,
        ) -> Result<Option<Value>, JsError> {
            if !reserve(100 + 24) {
                return Ok(Some(Value::Null));
            }
            self.note();
            Ok(Some(Value::String("x".repeat(100))))
        }

        fn get(&self, _object: &str, _property: &str) -> Value {
            self.note();
            Value::String("x".repeat(100))
        }

        fn get_with_budget(
            &self,
            _object: &str,
            _property: &str,
            reserve: &mut dyn FnMut(usize) -> bool,
        ) -> Option<Value> {
            if !reserve(100 + 24) {
                return Some(Value::Undefined);
            }
            self.note();
            Some(Value::String("x".repeat(100)))
        }
    }

    #[test]
    fn host_reserves_budget_before_materialising_strings() {
        let materialized = Rc::new(Cell::new(0));
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(BudgetHost { materialized: materialized.clone() }));
        runtime.set_memory_budget(1_000);
        runtime.set_memory_used(950);

        assert_eq!(runtime.test_host_call("host", "data", &[]).expect("call").display(), "null");
        assert_eq!(runtime.test_host_get("host", "data").expect("get").display(), "undefined");
        assert_eq!(materialized.get(), 0, "oversized host strings must not be materialised");
    }

    #[test]
    fn nested_host_objects_are_charged_recursively() {
        let mut runtime = Runtime::new();
        runtime.set_memory_budget(200);
        runtime.set_host(Box::new(NestedHost));
        runtime.define_global("host", Value::Host("host".into()));
        let error = runtime.eval("host.data").expect_err("nested object must exhaust the budget");
        assert!(error.message.contains("memory budget"), "unexpected error: {}", error.message);
    }

    #[test]
    fn cyclic_and_shared_host_objects_are_bounded() {
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(CyclicHost));
        runtime.define_global("host", Value::Host("host".into()));
        runtime.eval("host.node").expect("a cycle must not recurse forever");

        let mut runtime = Runtime::new();
        runtime.set_memory_budget(1500);
        runtime.set_host(Box::new(SharedHost));
        runtime.define_global("host", Value::Host("host".into()));
        runtime.eval("host.node").expect("a shared object must be charged once");
        assert!(runtime.memory_used() <= 1500);
    }

    #[test]
    fn host_display_reserves_the_budget_before_conversion() {
        let mut runtime = Runtime::new();
        runtime.set_memory_budget(100);
        runtime.set_memory_used(90);
        let error = runtime
            .charge_host_display(&Value::String("x".repeat(50)))
            .expect_err("conversion must be refused before allocation");
        assert!(error.message.contains("memory budget"), "unexpected error: {}", error.message);
        assert_eq!(runtime.memory_used(), 90);
    }

    #[test]
    fn rc_backed_values_are_not_double_charged() {
        let mut runtime = Runtime::new();
        runtime.eval("let a = [1, 2, 3];").expect("create array");
        let before = runtime.memory_used();
        runtime.eval("let b = a;").expect("alias the Rc-backed array");
        let delta = runtime.memory_used() - before;

        // Compare with aliasing a scalar: the only extra cost is the parsed
        // statement and the variable name, not the array's contents.
        let mut scalar = Runtime::new();
        scalar.eval("let a = 1;").expect("create scalar");
        let before_scalar = scalar.memory_used();
        scalar.eval("let b = a;").expect("alias the scalar");
        let scalar_delta = scalar.memory_used() - before_scalar;
        assert!(delta <= scalar_delta + 16, "aliasing an Rc value cost {delta} bytes");
    }

    #[test]
    fn rejected_allocations_do_not_change_memory_usage() {
        let mut runtime = Runtime::new();
        runtime.set_memory_budget(1_000);
        runtime.set_memory_used(950);
        let before = runtime.memory_used();
        let error = runtime
            .eval("'0123456789012345678901234567890123456789'")
            .expect_err("string allocation must fail");
        assert!(error.message.contains("memory budget"), "unexpected error: {}", error.message);
        assert_eq!(runtime.memory_used(), before);
    }

    #[test]
    fn reset_context_discards_old_globals_and_keeps_builtins_and_host() {
        let mut runtime = Runtime::new();
        runtime
            .eval("let secret = 'old'; function oldFn() { return 1; } print('old output');")
            .expect("first document");
        assert!(runtime.eval("secret").is_ok());
        assert!(runtime.memory_used() > 0);

        // The bridge is preserved by the reset; the shell then installs the
        // new document's host with set_host.
        runtime.set_host(Box::new(LargeStringHost));
        runtime.reset_context();

        assert_eq!(runtime.memory_used(), 0);
        assert!(runtime.output().is_empty());
        assert!(runtime.eval("secret").is_err(), "old global survived the reset");
        assert!(runtime.eval("oldFn").is_err(), "old function survived the reset");
        assert_eq!(runtime.eval("1 + 1").expect("built-ins").display(), "2");
        assert_eq!(runtime.eval("Math.floor(1.9)").expect("Math").display(), "1");
        runtime.define_global("host", Value::Host("host".into()));
        assert_eq!(
            runtime.eval("host.value").expect("host bridge preserved").display(),
            "x".repeat(128)
        );
    }

    #[test]
    fn repeated_function_creation_is_bounded() {
        let mut runtime = Runtime::new();
        runtime.set_memory_budget(2000);
        let error = runtime
            .eval("while (true) { let f = function() { return 1; }; }")
            .expect_err("AST copies must exhaust the memory budget");
        assert!(error.message.contains("memory budget"), "unexpected error: {}", error.message);
    }

    #[test]
    fn page_budget_resets_memory_accounting() {
        let mut runtime = Runtime::new();
        runtime.set_memory_budget(512);
        runtime.eval("let s = '0123456789';").expect("first document");
        let used = runtime.memory_used();
        assert!(used > 0);

        // Preparing the next document resets usage but keeps the configured
        // budget, so one page cannot consume the allowance of the next.
        runtime.set_page_budget(PAGE_INSTRUCTION_BUDGET);
        assert_eq!(runtime.memory_used(), 0);
        assert_eq!(runtime.memory_budget(), 512);
        runtime.eval("let t = '0123456789';").expect("second document");
    }
}
