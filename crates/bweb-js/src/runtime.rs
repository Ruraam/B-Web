//! Evaluator for the B-Web JavaScript subset.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
};

use crate::ast::{BinaryOp, Expr, LogicalOp, Stmt, UnaryOp, VarKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsError {
    pub message: String,
}

impl std::fmt::Display for JsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for JsError {}

pub type NativeFn = fn(&mut Runtime, Vec<Value>) -> Result<Value, JsError>;

#[derive(Clone)]
pub enum Value {
    Undefined,
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Object(Rc<RefCell<Object>>),
    Function(Rc<Function>),
    Native(NativeFn),
    Host(String),
}

impl std::fmt::Debug for Value {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "Value({})", self.display())
    }
}

impl Value {
    /// JavaScript-like string conversion used by `print` and concatenation.
    #[must_use]
    pub fn display(&self) -> String {
        match self {
            Self::Undefined => "undefined".into(),
            Self::Null => "null".into(),
            Self::Bool(true) => "true".into(),
            Self::Bool(false) => "false".into(),
            Self::Number(number) => {
                if number.is_nan() {
                    "NaN".into()
                } else if number.is_infinite() {
                    if *number > 0.0 { "Infinity".into() } else { "-Infinity".into() }
                } else if number.fract() == 0.0 {
                    format!("{}", *number as i64)
                } else {
                    number.to_string()
                }
            }
            Self::String(value) => value.clone(),
            Self::Object(_) | Self::Function(_) => "[object Object]".into(),
            Self::Native(_) => "function native() { [native code] }".into(),
            Self::Host(name) => format!("[host {name}]"),
        }
    }

    /// Upper bound on the byte length of `display`, without allocating. Exact
    /// for strings and host handles; a small conservative bound for the numeric
    /// and fixed representations.
    #[must_use]
    pub fn display_size_hint(&self) -> usize {
        match self {
            Self::String(text) => text.len(),
            Self::Host(name) => name.len().saturating_add(8),
            _ => 64,
        }
    }

    /// Converts to text only after checking the single-string size limit, so a
    /// host bridge cannot allocate an unbounded conversion.
    pub fn display_limited(&self, max_bytes: usize) -> Result<String, JsError> {
        if self.display_size_hint() > max_bytes {
            return Err(JsError { message: "maximum string length exceeded".into() });
        }
        Ok(self.display())
    }

    fn type_name(&self) -> &'static str {
        match self {
            Self::Undefined => "undefined",
            Self::Null => "object",
            Self::Bool(_) => "boolean",
            Self::Number(_) => "number",
            Self::String(_) => "string",
            Self::Object(_) | Self::Function(_) | Self::Native(_) | Self::Host(_) => "object",
        }
    }

    fn truthy(&self) -> bool {
        match self {
            Self::Undefined | Self::Null => false,
            Self::Bool(value) => *value,
            Self::Number(value) => *value != 0.0 && !value.is_nan(),
            Self::String(value) => !value.is_empty(),
            _ => true,
        }
    }

    fn number(&self) -> f64 {
        match self {
            Self::Undefined => f64::NAN,
            Self::Null => 0.0,
            Self::Bool(true) => 1.0,
            Self::Bool(false) => 0.0,
            Self::Number(value) => *value,
            Self::String(value) => value.trim().parse::<f64>().unwrap_or(f64::NAN),
            _ => f64::NAN,
        }
    }
}

#[derive(Default)]
pub struct Object {
    pub properties: HashMap<String, Value>,
    pub is_array: bool,
}

pub struct Function {
    params: Vec<String>,
    body: Vec<Stmt>,
    env: Env,
}

#[derive(Clone, Default)]
pub struct Env(Rc<RefCell<EnvData>>);

#[derive(Default)]
struct EnvData {
    values: HashMap<String, (Value, bool)>,
    parent: Option<Env>,
}

impl Env {
    fn child(parent: &Env) -> Self {
        Self(Rc::new(RefCell::new(EnvData { parent: Some(parent.clone()), ..EnvData::default() })))
    }

    fn define(&self, name: &str, value: Value, mutable: bool) {
        self.0.borrow_mut().values.insert(name.to_owned(), (value, mutable));
    }

    /// Looks up a variable, charging the clone before it is made.
    fn get_checked(
        &self,
        name: &str,
        charge: &mut impl FnMut(&Value) -> Result<(), JsError>,
    ) -> Result<Option<Value>, JsError> {
        let parent = {
            let data = self.0.borrow();
            if let Some((value, _)) = data.values.get(name) {
                charge(value)?;
                return Ok(Some(value.clone()));
            }
            data.parent.clone()
        };
        match parent {
            Some(parent) => parent.get_checked(name, charge),
            None => Ok(None),
        }
    }

    fn assign(&self, name: &str, value: Value) -> Result<(), JsError> {
        if let Some((existing, mutable)) = self.0.borrow_mut().values.get_mut(name) {
            if !*mutable {
                return Err(JsError { message: format!("assignment to constant '{name}'") });
            }
            *existing = value;
            return Ok(());
        }
        match self.0.borrow().parent.clone() {
            Some(parent) => parent.assign(name, value),
            None => Err(JsError { message: format!("undeclared variable '{name}'") }),
        }
    }
}

/// Host bridge used by scripts to interact with the embedding application.
pub trait Host {
    fn get(&self, _object: &str, _property: &str) -> Value {
        Value::Undefined
    }
    fn set(&mut self, _object: &str, _property: &str, _value: Value) -> Result<(), JsError> {
        Ok(())
    }
    fn call(&mut self, _object: &str, _method: &str, _args: &[Value]) -> Result<Value, JsError> {
        Ok(Value::Undefined)
    }
}

enum Flow {
    Normal,
    Return(Value),
    Break,
    Continue,
}

/// Global instruction budget per top-level evaluation or event callback.
const INSTRUCTION_BUDGET: u64 = 10_000_000;
/// Shared instruction budget for every script of one page (page load and event
/// callbacks). It complements, rather than replaces, the per-evaluation and
/// per-event budgets below.
pub const PAGE_INSTRUCTION_BUDGET: u64 = 10_000_000;
/// Cumulative token allowance shared by every script parsed for one page. It
/// bounds total parsing work independently of the instruction budget.
pub const PAGE_PARSE_TOKEN_BUDGET: usize = 1_000_000;
/// Smaller budget for interactive event handlers, which run directly on the
/// window's event loop. Measured at roughly 5 ms in release builds (10M ticks
/// take ~0.5 s), it keeps a handler within a frame's time slice.
pub const EVENT_HANDLER_INSTRUCTION_BUDGET: u64 = 100_000;
/// Maximum nested function calls before aborting (prevents stack overflow).
const MAX_CALL_DEPTH: usize = 64;
/// Cumulative allocation budget for one document. It is shared by every script
/// and event callback and bounds both retained data and allocation churn.
pub const PAGE_MEMORY_BUDGET: usize = 64 * 1024 * 1024;
/// Maximum size of a single string produced by a script or host value.
pub const MAX_STRING_BYTES: usize = 4 * 1024 * 1024;
/// Maximum number of elements in a script-created or host-created array.
pub const MAX_ARRAY_LENGTH: usize = 100_000;
/// Maximum number of own properties on a plain script or host object.
pub const MAX_OBJECT_PROPERTIES: usize = 10_000;
/// Fixed accounting cost of one heap allocation (string, property slot, value
/// clone or AST node).
const ALLOCATION_OVERHEAD: usize = 24;
/// Accounting cost of creating a child environment.
const ENV_MEMORY_COST: usize = 64;
/// Accounting cost of one AST node, independent of its payload.
const AST_NODE_MEMORY_COST: usize = 48;
/// Maximum nesting depth followed when charging objects returned by the host.
const MAX_HOST_VALUE_DEPTH: usize = 16;

/// A JavaScript interpreter instance.
pub struct Runtime {
    global: Env,
    host: Option<Box<dyn Host>>,
    output: Vec<String>,
    completion: Value,
    /// Remaining instructions for the current top-level evaluation.
    budget: u64,
    /// Current nested call depth.
    call_depth: usize,
    /// Optional remaining instruction allowance shared by all evaluations and
    /// event callbacks of the current page. `None` keeps the legacy behaviour.
    page_budget: Option<u64>,
    /// Optional remaining token allowance shared by all scripts parsed for the
    /// current page, including nested template interpolations.
    page_parse_budget: Option<usize>,
    /// Cumulative bytes charged to the current document.
    memory_used: usize,
    /// Maximum cumulative bytes for the current document.
    memory_budget: usize,
    /// Per-value limits, stored as fields so tests can use tiny bounds.
    max_string_bytes: usize,
    max_array_length: usize,
    max_object_properties: usize,
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

impl Runtime {
    #[must_use]
    pub fn new() -> Self {
        let global = Env::default();
        global.define("undefined", Value::Undefined, false);
        global.define("NaN", Value::Number(f64::NAN), false);
        global.define("Infinity", Value::Number(f64::INFINITY), false);
        global.define("print", Value::Native(native_print), false);
        global.define("parseInt", Value::Native(native_parse_int), false);
        global.define("parseFloat", Value::Native(native_parse_float), false);
        global.define("isNaN", Value::Native(native_is_nan), false);
        let math = Rc::new(RefCell::new(Object::default()));
        for (name, function) in [
            ("floor", native_floor as NativeFn),
            ("ceil", native_ceil),
            ("round", native_round),
            ("abs", native_abs),
            ("sqrt", native_sqrt),
            ("pow", native_pow),
            ("max", native_max),
            ("min", native_min),
            ("random", native_random),
        ] {
            math.borrow_mut().properties.insert(name.into(), Value::Native(function));
        }
        global.define("Math", Value::Object(math), false);
        let console = Rc::new(RefCell::new(Object::default()));
        console.borrow_mut().properties.insert("log".into(), Value::Native(native_print));
        global.define("console", Value::Object(console), false);
        Self {
            global,
            host: None,
            output: Vec::new(),
            completion: Value::Undefined,
            budget: INSTRUCTION_BUDGET,
            call_depth: 0,
            page_budget: None,
            page_parse_budget: None,
            memory_used: 0,
            memory_budget: PAGE_MEMORY_BUDGET,
            max_string_bytes: MAX_STRING_BYTES,
            max_array_length: MAX_ARRAY_LENGTH,
            max_object_properties: MAX_OBJECT_PROPERTIES,
        }
    }

    /// Enables the shared per-page budgets. Call this once per document: it
    /// resets the instruction, token and memory allowances. Each evaluation or
    /// event callback then draws from them in addition to its own limit.
    pub fn set_page_budget(&mut self, budget: u64) {
        self.page_budget = Some(budget);
        self.page_parse_budget = Some(PAGE_PARSE_TOKEN_BUDGET);
        self.memory_used = 0;
    }

    /// Removes the shared page budgets, restoring independent per-evaluation
    /// limits.
    pub fn clear_page_budget(&mut self) {
        self.page_budget = None;
        self.page_parse_budget = None;
    }

    /// Replaces the JavaScript context with a fresh one containing only the
    /// built-ins. Globals, functions, output, accumulated memory usage and the
    /// shared budgets are dropped; the host bridge and configured memory limits
    /// are kept. Call `set_host` for a new document and `set_page_budget` to
    /// enable its shared allowances.
    pub fn reset_context(&mut self) {
        let host = self.host.take();
        let memory_budget = self.memory_budget;
        let max_string_bytes = self.max_string_bytes;
        let max_array_length = self.max_array_length;
        let max_object_properties = self.max_object_properties;
        let fresh = Self::new();
        self.global = fresh.global;
        self.output = fresh.output;
        self.completion = fresh.completion;
        self.budget = fresh.budget;
        self.call_depth = fresh.call_depth;
        self.page_budget = fresh.page_budget;
        self.page_parse_budget = fresh.page_parse_budget;
        self.memory_used = fresh.memory_used;
        self.host = host;
        self.memory_budget = memory_budget;
        self.max_string_bytes = max_string_bytes;
        self.max_array_length = max_array_length;
        self.max_object_properties = max_object_properties;
    }

    /// Returns the remaining shared page instruction budget, if one is active.
    #[must_use]
    pub const fn page_budget_remaining(&self) -> Option<u64> {
        self.page_budget
    }

    /// Returns the remaining shared page parse allowance, if one is active.
    #[must_use]
    pub const fn page_parse_budget_remaining(&self) -> Option<usize> {
        self.page_parse_budget
    }

    /// Sets the cumulative allocation budget for the current document. Existing
    /// usage is retained, so shrinking the budget takes effect immediately.
    pub fn set_memory_budget(&mut self, budget: usize) {
        self.memory_budget = budget;
    }

    /// Returns the cumulative number of bytes charged to the current document.
    #[must_use]
    pub const fn memory_used(&self) -> usize {
        self.memory_used
    }

    /// Returns the cumulative allocation budget for the current document.
    #[must_use]
    pub const fn memory_budget(&self) -> usize {
        self.memory_budget
    }

    /// Overrides the per-value allocation limits, mainly for fast tests.
    #[cfg(test)]
    pub(crate) fn set_allocation_limits(
        &mut self,
        max_string_bytes: usize,
        max_array_length: usize,
        max_object_properties: usize,
    ) {
        self.max_string_bytes = max_string_bytes;
        self.max_array_length = max_array_length;
        self.max_object_properties = max_object_properties;
    }

    /// Sets the cumulative usage directly, mainly for boundary tests.
    #[cfg(test)]
    pub(crate) fn set_memory_used(&mut self, used: usize) {
        self.memory_used = used;
    }

    /// Verifies that an allocation of `bytes` fits the remaining budget without
    /// committing it, so callers can check before building a large value.
    fn ensure_memory(&self, bytes: usize) -> Result<(), JsError> {
        let used = self.memory_used.saturating_add(bytes);
        if used > self.memory_budget {
            return Err(JsError { message: "script memory budget exceeded".into() });
        }
        Ok(())
    }

    /// Charges a cumulative allocation, aborting once the document budget is
    /// exhausted.
    fn charge_memory(&mut self, bytes: usize) -> Result<(), JsError> {
        self.ensure_memory(bytes)?;
        self.memory_used = self.memory_used.saturating_add(bytes);
        Ok(())
    }

    /// Verifies that a string of `length` bytes is allowed before allocating it.
    fn ensure_string_len(&self, length: usize) -> Result<(), JsError> {
        if length > self.max_string_bytes {
            return Err(JsError { message: "maximum string length exceeded".into() });
        }
        self.ensure_memory(length.saturating_add(ALLOCATION_OVERHEAD))
    }

    /// Reserves the cost of a string before it is built.
    fn charge_string_len(&mut self, length: usize) -> Result<(), JsError> {
        self.ensure_string_len(length)?;
        self.charge_memory(length.saturating_add(ALLOCATION_OVERHEAD))
    }

    /// Charges a newly created string and enforces the single-value limit.
    fn charge_string(&mut self, value: &str) -> Result<(), JsError> {
        self.charge_string_len(value.len())
    }

    /// Converts a value to text, reserving the budget before the allocation.
    fn display_charged(&mut self, value: &Value) -> Result<String, JsError> {
        self.charge_string_len(value.display_size_hint())?;
        Ok(value.display())
    }

    /// Charges the copy performed when a value is cloned. Scalar values do not
    /// allocate; strings and host handles duplicate their payload.
    fn value_clone_cost(value: &Value) -> usize {
        match value {
            Value::String(text) => text.len().saturating_add(ALLOCATION_OVERHEAD),
            Value::Host(name) => name.len().saturating_add(ALLOCATION_OVERHEAD),
            _ => 0,
        }
    }

    fn charge_value_clone(&mut self, value: &Value) -> Result<(), JsError> {
        match value {
            Value::String(text) => self.charge_string(text),
            _ => self.charge_memory(Self::value_clone_cost(value)),
        }
    }

    /// Reserves the cost of a display conversion that the host bridge will
    /// perform on an argument. The bridge itself is not given the `Runtime`, so
    /// the charge is made here before handing the value over.
    pub fn charge_host_display(&mut self, value: &Value) -> Result<(), JsError> {
        self.charge_string_len(value.display_size_hint())
    }

    /// Charges values built by the host bridge and returned to scripts. Host
    /// objects are untrusted here: their array lengths, property counts and
    /// nested objects are validated with the same limits as script values.
    fn charge_host_value(&mut self, value: &Value) -> Result<(), JsError> {
        let mut visited = Vec::new();
        self.charge_host_value_inner(value, 0, &mut visited)
    }

    fn charge_host_value_inner(
        &mut self,
        value: &Value,
        depth: usize,
        visited: &mut Vec<*const RefCell<Object>>,
    ) -> Result<(), JsError> {
        match value {
            Value::String(text) => self.charge_string(text),
            Value::Host(name) => self.charge_memory(name.len().saturating_add(ALLOCATION_OVERHEAD)),
            Value::Object(object) => {
                if depth >= MAX_HOST_VALUE_DEPTH {
                    return Err(JsError { message: "host value nesting limit exceeded".into() });
                }
                let pointer = Rc::as_ptr(object);
                // A cycle or a shared object has already been charged at its
                // first occurrence; do not bill it again or recurse forever.
                if visited.contains(&pointer) {
                    return Ok(());
                }
                visited.push(pointer);
                {
                    let object = object.borrow();
                    if object.is_array {
                        let length = object.properties.get("length").map_or(0.0, Value::number);
                        if !length.is_finite()
                            || length < 0.0
                            || length > self.max_array_length as f64
                            || object.properties.len() > self.max_array_length.saturating_add(1)
                        {
                            return Err(JsError {
                                message: "maximum array length exceeded".into(),
                            });
                        }
                    } else if object.properties.len() > self.max_object_properties {
                        return Err(JsError {
                            message: "maximum object property count exceeded".into(),
                        });
                    }
                    let mut cost = ALLOCATION_OVERHEAD.saturating_mul(2).saturating_add(
                        object.properties.len().saturating_mul(ALLOCATION_OVERHEAD),
                    );
                    for (key, value) in &object.properties {
                        if key.len() > self.max_string_bytes {
                            return Err(JsError {
                                message: "maximum string length exceeded".into(),
                            });
                        }
                        cost = cost.saturating_add(key.len());
                        match value {
                            Value::String(text) => {
                                if text.len() > self.max_string_bytes {
                                    return Err(JsError {
                                        message: "maximum string length exceeded".into(),
                                    });
                                }
                                cost = cost.saturating_add(text.len());
                            }
                            Value::Host(name) => cost = cost.saturating_add(name.len()),
                            _ => {}
                        }
                    }
                    self.charge_memory(cost)?;
                }
                // Recurse after dropping the parent borrow. Cycle detection
                // above prevents re-entering an object already on the path.
                let nested: Vec<Value> = object
                    .borrow()
                    .properties
                    .values()
                    .filter(|value| matches!(value, Value::Object(_)))
                    .cloned()
                    .collect();
                for value in nested {
                    self.charge_host_value_inner(&value, depth + 1, visited)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Reads a script array length and enforces the array limit.
    fn array_length(&self, array: &Rc<RefCell<Object>>) -> Result<usize, JsError> {
        let length = array.borrow().properties.get("length").map_or(0.0, Value::number);
        if !length.is_finite() || length < 0.0 || length > self.max_array_length as f64 {
            return Err(JsError { message: "maximum array length exceeded".into() });
        }
        Ok(length as usize)
    }

    /// Inserts one property, enforcing object and array limits and charging the
    /// key allocation. Existing keys may be overwritten without growing.
    fn insert_property(
        &mut self,
        object: &Rc<RefCell<Object>>,
        key: String,
        value: Value,
    ) -> Result<(), JsError> {
        let (is_array, count, exists) = {
            let object = object.borrow();
            (object.is_array, object.properties.len(), object.properties.contains_key(&key))
        };
        if !exists {
            let limit = if is_array {
                self.max_array_length.saturating_add(1)
            } else {
                self.max_object_properties
            };
            if count >= limit {
                let message = if is_array {
                    "maximum array length exceeded"
                } else {
                    "maximum object property count exceeded"
                };
                return Err(JsError { message: message.into() });
            }
        }
        self.charge_memory(key.len().saturating_add(ALLOCATION_OVERHEAD))?;
        object.borrow_mut().properties.insert(key, value);
        Ok(())
    }

    /// Overrides the cumulative parse allowance, mainly for tests and callers
    /// that need a tighter bound than the default page budget.
    #[cfg(test)]
    pub(crate) fn set_page_parse_budget(&mut self, budget: usize) {
        self.page_parse_budget = Some(budget);
    }

    /// Reserves up to `limit` instructions from the shared page budget and
    /// returns the granted amount. Unused instructions are released afterwards.
    fn reserve_budget(&mut self, limit: u64) -> Result<u64, JsError> {
        let Some(remaining) = self.page_budget else {
            return Ok(limit);
        };
        if remaining == 0 {
            return Err(JsError { message: "page instruction budget exhausted".into() });
        }
        let granted = remaining.min(limit);
        self.page_budget = Some(remaining - granted);
        Ok(granted)
    }

    /// Returns instructions left by a completed evaluation to the page budget.
    fn release_budget(&mut self, granted: u64) {
        if let Some(remaining) = &mut self.page_budget {
            *remaining = remaining.saturating_add(self.budget.min(granted));
        }
    }

    pub fn set_host(&mut self, host: Box<dyn Host>) {
        self.host = Some(host);
    }

    pub fn define_global(&mut self, name: &str, value: Value) {
        self.global.define(name, value, false);
    }

    /// Text produced by `print` and `console.log`.
    #[must_use]
    pub fn output(&self) -> &[String] {
        &self.output
    }

    /// Drains accumulated script output.
    pub fn take_output(&mut self) -> Vec<String> {
        std::mem::take(&mut self.output)
    }

    /// Executes a parsed program and returns its completion value.
    pub fn run(&mut self, statements: &[Stmt]) -> Result<Value, JsError> {
        let mut env = self.global.clone();
        self.completion = Value::Undefined;
        match self.exec_statements(statements, &mut env)? {
            Flow::Return(value) => Ok(value),
            _ => {
                let cost = {
                    let value = &self.completion;
                    if let Value::String(text) = value {
                        if text.len() > self.max_string_bytes {
                            return Err(JsError {
                                message: "maximum string length exceeded".into(),
                            });
                        }
                    }
                    Self::value_clone_cost(value)
                };
                self.charge_memory(cost)?;
                Ok(self.completion.clone())
            }
        }
    }

    /// Parses and executes a source string.
    pub fn eval(&mut self, source: &str) -> Result<Value, JsError> {
        // Refuse further parsing as soon as the shared instruction allowance is
        // gone: parsing itself must not outlive the page budget.
        if self.page_budget == Some(0) {
            return Err(JsError { message: "page instruction budget exhausted".into() });
        }
        let statements = if let Some(remaining) = self.page_parse_budget {
            let budget = Rc::new(Cell::new(remaining));
            let parsed = crate::parser::parse_with_token_budget(source, budget.clone());
            self.page_parse_budget = Some(budget.get());
            parsed.map_err(|error| JsError { message: error.message })?
        } else {
            crate::parser::parse(source).map_err(|error| JsError { message: error.message })?
        };
        let granted = self.reserve_budget(INSTRUCTION_BUDGET)?;
        self.budget = granted;
        self.call_depth = 0;
        let result = self.run(&statements);
        self.release_budget(granted);
        result
    }

    fn exec_statements(&mut self, statements: &[Stmt], env: &mut Env) -> Result<Flow, JsError> {
        for statement in statements {
            let flow = self.exec_statement(statement, env)?;
            if !matches!(flow, Flow::Normal) {
                return Ok(flow);
            }
        }
        Ok(Flow::Normal)
    }

    /// Consumes one unit of the execution budget. Returns an error when the
    /// budget is exhausted, aborting the current evaluation.
    fn tick(&mut self) -> Result<(), JsError> {
        if self.budget == 0 {
            return Err(JsError { message: "script instruction budget exceeded".into() });
        }
        self.budget -= 1;
        Ok(())
    }

    fn exec_statement(&mut self, statement: &Stmt, env: &mut Env) -> Result<Flow, JsError> {
        self.tick()?;
        match statement {
            Stmt::Empty => Ok(Flow::Normal),
            Stmt::Expression(expression) => {
                self.completion = self.eval_expr(expression, env)?;
                Ok(Flow::Normal)
            }
            Stmt::Var { kind, name, value } => {
                let value = match value {
                    Some(expression) => self.eval_expr(expression, env)?,
                    None => Value::Undefined,
                };
                self.charge_memory(name.len().saturating_add(ALLOCATION_OVERHEAD))?;
                env.define(name, value, !matches!(kind, VarKind::Const));
                Ok(Flow::Normal)
            }
            Stmt::Function { name, params, body } => {
                self.charge_memory(function_memory_cost(params, body))?;
                self.charge_memory(name.len().saturating_add(ALLOCATION_OVERHEAD))?;
                let function = Value::Function(Rc::new(Function {
                    params: params.clone(),
                    body: body.clone(),
                    env: env.clone(),
                }));
                env.define(name, function, true);
                Ok(Flow::Normal)
            }
            Stmt::Block(statements) => {
                self.charge_memory(ENV_MEMORY_COST)?;
                let mut child = Env::child(env);
                self.exec_statements(statements, &mut child)
            }
            Stmt::If { test, then, otherwise } => {
                if self.eval_expr(test, env)?.truthy() {
                    self.exec_statement(then, env)
                } else if let Some(otherwise) = otherwise {
                    self.exec_statement(otherwise, env)
                } else {
                    Ok(Flow::Normal)
                }
            }
            Stmt::While { test, body } => {
                let mut iterations = 0usize;
                while self.eval_expr(test, env)?.truthy() {
                    iterations += 1;
                    if iterations > 100_000 {
                        return Err(JsError { message: "loop iteration limit exceeded".into() });
                    }
                    match self.exec_statement(body, env)? {
                        Flow::Break => break,
                        Flow::Return(value) => return Ok(Flow::Return(value)),
                        _ => {}
                    }
                }
                Ok(Flow::Normal)
            }
            Stmt::For { init, test, update, body } => {
                self.charge_memory(ENV_MEMORY_COST)?;
                let mut loop_env = Env::child(env);
                if let Some(init) = init {
                    self.exec_statement(init, &mut loop_env)?;
                }
                let mut iterations = 0usize;
                loop {
                    if let Some(test) = test {
                        if !self.eval_expr(test, &mut loop_env)?.truthy() {
                            break;
                        }
                    }
                    iterations += 1;
                    if iterations > 100_000 {
                        return Err(JsError { message: "loop iteration limit exceeded".into() });
                    }
                    match self.exec_statement(body, &mut loop_env)? {
                        Flow::Break => break,
                        Flow::Return(value) => return Ok(Flow::Return(value)),
                        _ => {}
                    }
                    if let Some(update) = update {
                        self.eval_expr(update, &mut loop_env)?;
                    }
                }
                Ok(Flow::Normal)
            }
            Stmt::Return(value) => {
                let value = match value {
                    Some(expression) => self.eval_expr(expression, env)?,
                    None => Value::Undefined,
                };
                Ok(Flow::Return(value))
            }
            Stmt::Break => Ok(Flow::Break),
            Stmt::Continue => Ok(Flow::Continue),
        }
    }

    fn eval_expr(&mut self, expression: &Expr, env: &mut Env) -> Result<Value, JsError> {
        self.tick()?;
        match expression {
            Expr::Undefined => Ok(Value::Undefined),
            Expr::Null => Ok(Value::Null),
            Expr::Bool(value) => Ok(Value::Bool(*value)),
            Expr::Number(value) => Ok(Value::Number(*value)),
            Expr::String(value) => {
                if value.len() > self.max_string_bytes {
                    return Err(JsError { message: "maximum string length exceeded".into() });
                }
                self.charge_string(value)?;
                Ok(Value::String(value.clone()))
            }
            Expr::Template(parts) => {
                self.charge_memory(parts.len().saturating_mul(std::mem::size_of::<String>()))?;
                let mut rendered = Vec::with_capacity(parts.len());
                let mut total = 0usize;
                for part in parts {
                    let value = self.eval_expr(part, env)?;
                    let text = self.display_charged(&value)?;
                    total = total.saturating_add(text.len());
                    rendered.push(text);
                }
                self.ensure_string_len(total)?;
                let mut text = String::with_capacity(total);
                for part in rendered {
                    text.push_str(&part);
                }
                self.charge_string_len(total)?;
                Ok(Value::String(text))
            }
            Expr::Identifier(name) => {
                let value = env
                    .get_checked(name, &mut |value| self.charge_value_clone(value))?
                    .ok_or_else(|| JsError { message: format!("'{name}' is not defined") })?;
                Ok(value)
            }
            Expr::Array(values) => {
                if values.len() > self.max_array_length {
                    return Err(JsError { message: "maximum array length exceeded".into() });
                }
                self.charge_memory(ALLOCATION_OVERHEAD)?;
                let object = Rc::new(RefCell::new(Object { is_array: true, ..Object::default() }));
                for (index, value) in values.iter().enumerate() {
                    let value = self.eval_expr(value, env)?;
                    self.insert_property(&object, index.to_string(), value)?;
                }
                self.insert_property(&object, "length".into(), Value::Number(values.len() as f64))?;
                Ok(Value::Object(object))
            }
            Expr::Object(properties) => {
                if properties.len() > self.max_object_properties {
                    return Err(JsError {
                        message: "maximum object property count exceeded".into(),
                    });
                }
                self.charge_memory(ALLOCATION_OVERHEAD)?;
                let object = Rc::new(RefCell::new(Object::default()));
                for (name, value) in properties {
                    let value = self.eval_expr(value, env)?;
                    self.insert_property(&object, name.clone(), value)?;
                }
                Ok(Value::Object(object))
            }
            Expr::Function { params, body } => {
                self.charge_memory(function_memory_cost(params, body))?;
                Ok(Value::Function(Rc::new(Function {
                    params: params.clone(),
                    body: body.clone(),
                    env: env.clone(),
                })))
            }
            Expr::Unary { op, value } => {
                let value = self.eval_expr(value, env)?;
                match op {
                    UnaryOp::Negate => Ok(Value::Number(-value.number())),
                    UnaryOp::Not => Ok(Value::Bool(!value.truthy())),
                    UnaryOp::TypeOf => Ok(Value::String(value.type_name().into())),
                }
            }
            Expr::Binary { op, left, right } => {
                let left = self.eval_expr(left, env)?;
                let right = self.eval_expr(right, env)?;
                self.binary(op, left, right)
            }
            Expr::Logical { op, left, right } => {
                let left = self.eval_expr(left, env)?;
                match op {
                    LogicalOp::Or if left.truthy() => Ok(left),
                    LogicalOp::And if !left.truthy() => Ok(left),
                    _ => self.eval_expr(right, env),
                }
            }
            Expr::Conditional { test, then, otherwise } => {
                if self.eval_expr(test, env)?.truthy() {
                    self.eval_expr(then, env)
                } else {
                    self.eval_expr(otherwise, env)
                }
            }
            Expr::Assign { target, value } => {
                let value = self.eval_expr(value, env)?;
                self.charge_value_clone(&value)?;
                self.assign(target, value.clone(), env)?;
                Ok(value)
            }
            Expr::Member { object, property } => {
                let object = self.eval_expr(object, env)?;
                let property = self.eval_expr(property, env)?;
                let property = self.display_charged(&property)?;
                self.get_member(&object, &property)
            }
            Expr::Call { callee, args } => {
                let mut values = Vec::new();
                for arg in args {
                    values.push(self.eval_expr(arg, env)?);
                }
                if let Expr::Member { object, property } = &**callee {
                    let target = self.eval_expr(object, env)?;
                    let method = self.eval_expr(property, env)?;
                    let method = self.display_charged(&method)?;
                    match target {
                        Value::Host(name) => return self.call_host(&name, &method, &values),
                        Value::String(text) => {
                            return string_method(self, &text, &method, &values);
                        }
                        Value::Object(object) if object.borrow().is_array => {
                            return array_method(self, &object, &method, &values);
                        }
                        other => {
                            let function = self.get_member(&other, &method)?;
                            return self.call_value(function, values);
                        }
                    }
                }
                let callee = self.eval_expr(callee, env)?;
                self.call_value(callee, values)
            }
        }
    }

    fn get_member(&mut self, object: &Value, property: &str) -> Result<Value, JsError> {
        match object {
            Value::Host(name) => {
                let value =
                    self.host.as_ref().map_or(Value::Undefined, |host| host.get(name, property));
                self.charge_host_value(&value)?;
                Ok(value)
            }
            Value::Object(object) => {
                let value = {
                    let object = object.borrow();
                    let Some(value) = object.properties.get(property) else {
                        return Ok(Value::Undefined);
                    };
                    self.charge_value_clone(value)?;
                    value.clone()
                };
                Ok(value)
            }
            Value::String(value) if property == "length" => {
                Ok(Value::Number(value.chars().count() as f64))
            }
            _ => Ok(Value::Undefined),
        }
    }

    fn assign(&mut self, target: &Expr, value: Value, env: &mut Env) -> Result<(), JsError> {
        match target {
            Expr::Identifier(name) => env.assign(name, value),
            Expr::Member { object, property } => {
                let object = self.eval_expr(object, env)?;
                let property = self.eval_expr(property, env)?;
                let property = self.display_charged(&property)?;
                match object {
                    Value::Host(name) => {
                        self.charge_host_display(&value)?;
                        match self.host.as_mut() {
                            Some(host) => host.set(&name, &property, value),
                            None => Ok(()),
                        }
                    }
                    Value::Object(object) => {
                        if object.borrow().is_array && property == "length" {
                            let length = value.number();
                            if !length.is_finite()
                                || length < 0.0
                                || length > self.max_array_length as f64
                            {
                                return Err(JsError {
                                    message: "maximum array length exceeded".into(),
                                });
                            }
                        }
                        self.insert_property(&object, property, value)
                    }
                    _ => Ok(()),
                }
            }
            _ => Err(JsError { message: "invalid assignment target".into() }),
        }
    }

    fn call_host(&mut self, host: &str, method: &str, args: &[Value]) -> Result<Value, JsError> {
        // A host bridge converts its arguments with `Value::display` internally;
        // reserve that cost before it can allocate the converted strings.
        for argument in args {
            self.charge_host_display(argument)?;
        }
        let value = match self.host.as_mut() {
            Some(bridge) => bridge.call(host, method, args)?,
            None => return Ok(Value::Undefined),
        };
        self.charge_host_value(&value)?;
        Ok(value)
    }

    /// Calls a runtime value as a function with the full script budget.
    /// Each dispatch gets a fresh budget so one handler cannot starve another.
    pub fn call(&mut self, callee: Value, args: Vec<Value>) -> Result<Value, JsError> {
        self.call_with_budget(callee, args, INSTRUCTION_BUDGET)
    }

    /// Calls an interactive event handler with the smaller per-frame budget.
    pub fn call_event(&mut self, callee: Value, args: Vec<Value>) -> Result<Value, JsError> {
        self.call_with_budget(callee, args, EVENT_HANDLER_INSTRUCTION_BUDGET)
    }

    /// Calls a function with an explicit instruction budget. Both the budget
    /// and the call depth are reset, so a failed call cannot affect the next
    /// evaluation.
    pub fn call_with_budget(
        &mut self,
        callee: Value,
        args: Vec<Value>,
        budget: u64,
    ) -> Result<Value, JsError> {
        let granted = self.reserve_budget(budget)?;
        self.budget = granted;
        self.call_depth = 0;
        let result = self.call_value(callee, args);
        self.release_budget(granted);
        result
    }

    fn call_value(&mut self, callee: Value, args: Vec<Value>) -> Result<Value, JsError> {
        self.tick()?;
        self.charge_memory(args.len().saturating_mul(std::mem::size_of::<Value>()))?;
        match callee {
            Value::Native(function) => function(self, args),
            Value::Function(function) => {
                if self.call_depth >= MAX_CALL_DEPTH {
                    return Err(JsError { message: "maximum call depth exceeded".into() });
                }
                self.call_depth += 1;
                self.charge_memory(ENV_MEMORY_COST)?;
                let mut env = Env::child(&function.env);
                for (index, param) in function.params.iter().enumerate() {
                    let value = match args.get(index) {
                        Some(value) => {
                            self.charge_value_clone(value)?;
                            value.clone()
                        }
                        None => Value::Undefined,
                    };
                    self.charge_memory(param.len().saturating_add(ALLOCATION_OVERHEAD))?;
                    env.define(param, value, true);
                }
                let result = self.exec_statements(&function.body, &mut env);
                self.call_depth -= 1;
                match result? {
                    Flow::Return(value) => Ok(value),
                    _ => Ok(Value::Undefined),
                }
            }
            other => Err(JsError { message: format!("'{}' is not a function", other.display()) }),
        }
    }

    fn binary(&mut self, op: &BinaryOp, left: Value, right: Value) -> Result<Value, JsError> {
        let result = match op {
            BinaryOp::Add => {
                if matches!(&left, Value::String(_)) || matches!(&right, Value::String(_)) {
                    let left_text = self.display_charged(&left)?;
                    let right_text = self.display_charged(&right)?;
                    let total = left_text.len().saturating_add(right_text.len());
                    self.ensure_string_len(total)?;
                    let mut text = String::with_capacity(total);
                    text.push_str(&left_text);
                    text.push_str(&right_text);
                    self.charge_string_len(total)?;
                    Value::String(text)
                } else {
                    Value::Number(left.number() + right.number())
                }
            }
            BinaryOp::Subtract => Value::Number(left.number() - right.number()),
            BinaryOp::Multiply => Value::Number(left.number() * right.number()),
            BinaryOp::Divide => Value::Number(left.number() / right.number()),
            BinaryOp::Modulo => Value::Number(left.number() % right.number()),
            BinaryOp::Equals => Value::Bool(loose_equals(&left, &right)),
            BinaryOp::NotEquals => Value::Bool(!loose_equals(&left, &right)),
            BinaryOp::StrictEquals => Value::Bool(strict_equals(&left, &right)),
            BinaryOp::StrictNotEquals => Value::Bool(!strict_equals(&left, &right)),
            BinaryOp::Less => Value::Bool(left.number() < right.number()),
            BinaryOp::LessOrEqual => Value::Bool(left.number() <= right.number()),
            BinaryOp::Greater => Value::Bool(left.number() > right.number()),
            BinaryOp::GreaterOrEqual => Value::Bool(left.number() >= right.number()),
        };
        Ok(result)
    }
}

fn native_print(runtime: &mut Runtime, args: Vec<Value>) -> Result<Value, JsError> {
    runtime.charge_memory(args.len().saturating_mul(std::mem::size_of::<String>()))?;
    let mut parts = Vec::with_capacity(args.len());
    let mut total = 0usize;
    for arg in &args {
        let part = runtime.display_charged(arg)?;
        total = total.saturating_add(part.len());
        parts.push(part);
    }
    if args.len() > 1 {
        total = total.saturating_add(args.len() - 1);
    }
    runtime.ensure_string_len(total)?;
    let mut text = String::with_capacity(total);
    for (index, part) in parts.into_iter().enumerate() {
        if index > 0 {
            text.push(' ');
        }
        text.push_str(&part);
    }
    runtime.charge_string_len(total)?;
    runtime.output.push(text);
    Ok(Value::Undefined)
}

fn native_floor(_: &mut Runtime, args: Vec<Value>) -> Result<Value, JsError> {
    Ok(Value::Number(args.first().map_or(f64::NAN, Value::number).floor()))
}

fn native_ceil(_: &mut Runtime, args: Vec<Value>) -> Result<Value, JsError> {
    Ok(Value::Number(args.first().map_or(f64::NAN, Value::number).ceil()))
}

fn native_round(_: &mut Runtime, args: Vec<Value>) -> Result<Value, JsError> {
    Ok(Value::Number(args.first().map_or(f64::NAN, Value::number).round()))
}

fn native_abs(_: &mut Runtime, args: Vec<Value>) -> Result<Value, JsError> {
    Ok(Value::Number(args.first().map_or(f64::NAN, Value::number).abs()))
}

fn native_sqrt(_: &mut Runtime, args: Vec<Value>) -> Result<Value, JsError> {
    Ok(Value::Number(args.first().map_or(f64::NAN, Value::number).sqrt()))
}

fn native_pow(_: &mut Runtime, args: Vec<Value>) -> Result<Value, JsError> {
    let base = args.first().map_or(f64::NAN, Value::number);
    let exponent = args.get(1).map_or(f64::NAN, Value::number);
    Ok(Value::Number(base.powf(exponent)))
}

fn native_max(_: &mut Runtime, args: Vec<Value>) -> Result<Value, JsError> {
    Ok(Value::Number(args.iter().map(Value::number).fold(f64::NEG_INFINITY, f64::max)))
}

fn native_min(_: &mut Runtime, args: Vec<Value>) -> Result<Value, JsError> {
    Ok(Value::Number(args.iter().map(Value::number).fold(f64::INFINITY, f64::min)))
}

fn native_random(_: &mut Runtime, _: Vec<Value>) -> Result<Value, JsError> {
    Ok(Value::Number(0.5))
}

fn native_parse_int(runtime: &mut Runtime, args: Vec<Value>) -> Result<Value, JsError> {
    let value = match args.first() {
        Some(value) => runtime.display_charged(value)?,
        None => String::new(),
    };
    let value = value.trim();
    let end = value
        .char_indices()
        .find(|(_, character)| {
            !character.is_ascii_digit() && *character != '-' && *character != '+'
        })
        .map_or(value.len(), |(index, _)| index);
    Ok(Value::Number(value[..end].parse::<f64>().unwrap_or(f64::NAN).trunc()))
}

fn native_parse_float(runtime: &mut Runtime, args: Vec<Value>) -> Result<Value, JsError> {
    let value = match args.first() {
        Some(value) => runtime.display_charged(value)?,
        None => String::new(),
    };
    Ok(Value::Number(value.trim().parse::<f64>().unwrap_or(f64::NAN)))
}

fn native_is_nan(_: &mut Runtime, args: Vec<Value>) -> Result<Value, JsError> {
    Ok(Value::Bool(args.first().is_none_or(|value| value.number().is_nan())))
}

fn string_method(
    runtime: &mut Runtime,
    value: &str,
    method: &str,
    args: &[Value],
) -> Result<Value, JsError> {
    // Compute the exact byte length of the result before building it, so an
    // over-budget conversion is refused without allocating the large string.
    let planned_len = match method {
        "toUpperCase" => value.chars().flat_map(char::to_uppercase).map(char::len_utf8).sum(),
        "toLowerCase" => value.chars().flat_map(char::to_lowercase).map(char::len_utf8).sum(),
        "trim" => value.trim().len(),
        "charAt" => {
            let index = args.first().map_or(0.0, Value::number).max(0.0) as usize;
            value.chars().nth(index).map_or(0, char::len_utf8)
        }
        "slice" => {
            let start = args.first().map_or(0.0, Value::number).max(0.0) as usize;
            let end =
                args.get(1).map_or(value.chars().count(), |value| value.number().max(0.0) as usize);
            value.chars().skip(start).take(end.saturating_sub(start)).map(char::len_utf8).sum()
        }
        _ => 0,
    };
    runtime.ensure_string_len(planned_len)?;
    let result = match method {
        "toUpperCase" => Value::String(value.to_uppercase()),
        "toLowerCase" => Value::String(value.to_lowercase()),
        "trim" => Value::String(value.trim().to_owned()),
        "charAt" => {
            let index = args.first().map_or(0.0, Value::number).max(0.0) as usize;
            Value::String(value.chars().nth(index).map_or(String::new(), |ch| ch.to_string()))
        }
        "indexOf" => {
            let needle = match args.first() {
                Some(argument) => runtime.display_charged(argument)?,
                None => String::new(),
            };
            let index =
                value.find(&needle).map_or(-1.0, |offset| value[..offset].chars().count() as f64);
            Value::Number(index)
        }
        "slice" => {
            let start = args.first().map_or(0.0, Value::number).max(0.0) as usize;
            let end =
                args.get(1).map_or(value.chars().count(), |value| value.number().max(0.0) as usize);
            Value::String(value.chars().skip(start).take(end.saturating_sub(start)).collect())
        }
        _ => Value::Undefined,
    };
    if let Value::String(text) = &result {
        runtime.charge_string(text)?;
    }
    Ok(result)
}

fn array_method(
    runtime: &mut Runtime,
    array: &Rc<RefCell<Object>>,
    method: &str,
    args: &[Value],
) -> Result<Value, JsError> {
    match method {
        "push" => {
            let value = match args.first() {
                Some(value) => {
                    runtime.charge_value_clone(value)?;
                    value.clone()
                }
                None => Value::Undefined,
            };
            let length = runtime.array_length(array)?;
            if length >= runtime.max_array_length {
                return Err(JsError { message: "maximum array length exceeded".into() });
            }
            runtime.insert_property(array, length.to_string(), value)?;
            runtime.insert_property(array, "length".into(), Value::Number((length + 1) as f64))?;
            Ok(Value::Number((length + 1) as f64))
        }
        "join" => {
            let separator = match args.first() {
                Some(value) => runtime.display_charged(value)?,
                None => String::new(),
            };
            let length = runtime.array_length(array)?;
            // The temporary vector of rendered elements is itself an
            // allocation proportional to the array length.
            runtime.charge_memory(length.saturating_mul(ALLOCATION_OVERHEAD))?;
            let mut values = Vec::with_capacity(length);
            let mut total = 0usize;
            for index in 0..length {
                let item = {
                    let array = array.borrow();
                    match array.properties.get(&index.to_string()) {
                        Some(item) => {
                            runtime.charge_value_clone(item)?;
                            item.clone()
                        }
                        None => Value::Undefined,
                    }
                };
                let text = runtime.display_charged(&item)?;
                total = total.saturating_add(text.len());
                values.push(text);
            }
            if length > 1 {
                total = total.saturating_add(separator.len().saturating_mul(length - 1));
            }
            runtime.ensure_string_len(total)?;
            let joined = values.join(&separator);
            runtime.charge_string_len(total)?;
            Ok(Value::String(joined))
        }
        "forEach" | "map" => {
            let callback = match args.first() {
                Some(value) => {
                    runtime.charge_value_clone(value)?;
                    value.clone()
                }
                None => Value::Undefined,
            };
            let length = runtime.array_length(array)?;
            let output = if method == "map" {
                runtime.charge_memory(ALLOCATION_OVERHEAD)?;
                let output = Rc::new(RefCell::new(Object { is_array: true, ..Object::default() }));
                runtime.insert_property(&output, "length".into(), Value::Number(length as f64))?;
                Some(output)
            } else {
                None
            };
            for index in 0..length {
                let item = {
                    let array = array.borrow();
                    match array.properties.get(&index.to_string()) {
                        Some(item) => {
                            runtime.charge_value_clone(item)?;
                            item.clone()
                        }
                        None => Value::Undefined,
                    }
                };
                let result = runtime.call_value(
                    callback.clone(),
                    vec![item, Value::Number(index as f64), Value::Object(array.clone())],
                )?;
                if let Some(output) = &output {
                    runtime.insert_property(output, index.to_string(), result)?;
                }
            }
            if let Some(output) = output { Ok(Value::Object(output)) } else { Ok(Value::Undefined) }
        }
        _ => Ok(Value::Undefined),
    }
}

fn strict_equals(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Undefined, Value::Undefined) | (Value::Null, Value::Null) => true,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Number(a), Value::Number(b)) => a == b,
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Object(a), Value::Object(b)) => Rc::ptr_eq(a, b),
        (Value::Function(a), Value::Function(b)) => Rc::ptr_eq(a, b),
        (Value::Host(a), Value::Host(b)) => a == b,
        _ => false,
    }
}

fn loose_equals(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Null, Value::Undefined) | (Value::Undefined, Value::Null) => true,
        (
            Value::Number(_) | Value::String(_) | Value::Bool(_),
            Value::Number(_) | Value::String(_) | Value::Bool(_),
        ) => left.number() == right.number(),
        _ => strict_equals(left, right),
    }
}

/// Estimates the heap cost of cloning one function value, including its
/// captured environment reference, parameter names and AST body.
fn function_memory_cost(params: &[String], body: &[Stmt]) -> usize {
    let mut cost = ALLOCATION_OVERHEAD.saturating_mul(4);
    for param in params {
        cost = cost.saturating_add(ALLOCATION_OVERHEAD).saturating_add(param.len());
    }
    for statement in body {
        cost = cost.saturating_add(stmt_ast_cost(statement));
    }
    cost
}

fn expr_ast_cost(expression: &Expr) -> usize {
    let base = AST_NODE_MEMORY_COST;
    match expression {
        Expr::Undefined | Expr::Null | Expr::Bool(_) | Expr::Number(_) => base,
        Expr::String(value) => base.saturating_add(value.len()),
        Expr::Template(parts) => {
            parts.iter().fold(base, |cost, part| cost.saturating_add(expr_ast_cost(part)))
        }
        Expr::Identifier(name) => base.saturating_add(name.len()),
        Expr::Array(values) => {
            values.iter().fold(base, |cost, value| cost.saturating_add(expr_ast_cost(value)))
        }
        Expr::Object(properties) => properties.iter().fold(base, |cost, (name, value)| {
            cost.saturating_add(ALLOCATION_OVERHEAD)
                .saturating_add(name.len())
                .saturating_add(expr_ast_cost(value))
        }),
        Expr::Function { params, body } => base.saturating_add(function_memory_cost(params, body)),
        Expr::Unary { value, .. } => base.saturating_add(expr_ast_cost(value)),
        Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
            base.saturating_add(expr_ast_cost(left)).saturating_add(expr_ast_cost(right))
        }
        Expr::Assign { target, value } => {
            base.saturating_add(expr_ast_cost(target)).saturating_add(expr_ast_cost(value))
        }
        Expr::Conditional { test, then, otherwise } => base
            .saturating_add(expr_ast_cost(test))
            .saturating_add(expr_ast_cost(then))
            .saturating_add(expr_ast_cost(otherwise)),
        Expr::Member { object, property } => {
            base.saturating_add(expr_ast_cost(object)).saturating_add(expr_ast_cost(property))
        }
        Expr::Call { callee, args } => {
            args.iter().fold(base.saturating_add(expr_ast_cost(callee)), |cost, argument| {
                cost.saturating_add(expr_ast_cost(argument))
            })
        }
    }
}

fn stmt_ast_cost(statement: &Stmt) -> usize {
    let base = AST_NODE_MEMORY_COST;
    match statement {
        Stmt::Empty | Stmt::Break | Stmt::Continue => base,
        Stmt::Var { name, value, .. } => {
            let cost = base.saturating_add(name.len());
            value.as_ref().map_or(cost, |value| cost.saturating_add(expr_ast_cost(value)))
        }
        Stmt::Expression(expression) => base.saturating_add(expr_ast_cost(expression)),
        Stmt::Block(statements) => statements
            .iter()
            .fold(base, |cost, statement| cost.saturating_add(stmt_ast_cost(statement))),
        Stmt::If { test, then, otherwise } => {
            let cost = base.saturating_add(expr_ast_cost(test)).saturating_add(stmt_ast_cost(then));
            otherwise
                .as_ref()
                .map_or(cost, |otherwise| cost.saturating_add(stmt_ast_cost(otherwise)))
        }
        Stmt::While { test, body } => {
            base.saturating_add(expr_ast_cost(test)).saturating_add(stmt_ast_cost(body))
        }
        Stmt::For { init, test, update, body } => {
            let mut cost = base.saturating_add(stmt_ast_cost(body));
            if let Some(init) = init {
                cost = cost.saturating_add(stmt_ast_cost(init));
            }
            if let Some(test) = test {
                cost = cost.saturating_add(expr_ast_cost(test));
            }
            if let Some(update) = update {
                cost = cost.saturating_add(expr_ast_cost(update));
            }
            cost
        }
        Stmt::Function { name, params, body } => {
            base.saturating_add(name.len()).saturating_add(function_memory_cost(params, body))
        }
        Stmt::Return(value) => {
            let cost = base;
            value.as_ref().map_or(cost, |value| cost.saturating_add(expr_ast_cost(value)))
        }
    }
}
