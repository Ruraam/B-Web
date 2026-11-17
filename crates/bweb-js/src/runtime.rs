//! Evaluator for the B-Web JavaScript subset.

use std::{cell::RefCell, collections::HashMap, rc::Rc};

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

    fn get(&self, name: &str) -> Option<Value> {
        let data = self.0.borrow();
        if let Some((value, _)) = data.values.get(name) {
            return Some(value.clone());
        }
        data.parent.as_ref().and_then(|parent| parent.get(name))
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
/// Smaller budget for interactive event handlers, which run directly on the
/// window's event loop. Measured at roughly 5 ms in release builds (10M ticks
/// take ~0.5 s), it keeps a handler within a frame's time slice.
pub const EVENT_HANDLER_INSTRUCTION_BUDGET: u64 = 100_000;
/// Maximum nested function calls before aborting (prevents stack overflow).
const MAX_CALL_DEPTH: usize = 64;

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
            _ => Ok(self.completion.clone()),
        }
    }

    /// Parses and executes a source string.
    pub fn eval(&mut self, source: &str) -> Result<Value, JsError> {
        let statements =
            crate::parser::parse(source).map_err(|error| JsError { message: error.message })?;
        self.budget = INSTRUCTION_BUDGET;
        self.call_depth = 0;
        self.run(&statements)
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
                env.define(name, value, !matches!(kind, VarKind::Const));
                Ok(Flow::Normal)
            }
            Stmt::Function { name, params, body } => {
                let function = Value::Function(Rc::new(Function {
                    params: params.clone(),
                    body: body.clone(),
                    env: env.clone(),
                }));
                env.define(name, function, true);
                Ok(Flow::Normal)
            }
            Stmt::Block(statements) => {
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
            Expr::String(value) => Ok(Value::String(value.clone())),
            Expr::Template(parts) => {
                let mut text = String::new();
                for part in parts {
                    text.push_str(&self.eval_expr(part, env)?.display());
                }
                Ok(Value::String(text))
            }
            Expr::Identifier(name) => {
                env.get(name).ok_or_else(|| JsError { message: format!("'{name}' is not defined") })
            }
            Expr::Array(values) => {
                let object = Rc::new(RefCell::new(Object::default()));
                let mut length = 0usize;
                for value in values {
                    let value = self.eval_expr(value, env)?;
                    object.borrow_mut().properties.insert(length.to_string(), value);
                    length += 1;
                }
                {
                    let mut object = object.borrow_mut();
                    object.is_array = true;
                    object.properties.insert("length".into(), Value::Number(length as f64));
                }
                Ok(Value::Object(object))
            }
            Expr::Object(properties) => {
                let object = Rc::new(RefCell::new(Object::default()));
                for (name, value) in properties {
                    let value = self.eval_expr(value, env)?;
                    object.borrow_mut().properties.insert(name.clone(), value);
                }
                Ok(Value::Object(object))
            }
            Expr::Function { params, body } => Ok(Value::Function(Rc::new(Function {
                params: params.clone(),
                body: body.clone(),
                env: env.clone(),
            }))),
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
                self.assign(target, value.clone(), env)?;
                Ok(value)
            }
            Expr::Member { object, property } => {
                let object = self.eval_expr(object, env)?;
                let property = self.eval_expr(property, env)?.display();
                self.get_member(&object, &property)
            }
            Expr::Call { callee, args } => {
                let mut values = Vec::new();
                for arg in args {
                    values.push(self.eval_expr(arg, env)?);
                }
                if let Expr::Member { object, property } = &**callee {
                    let target = self.eval_expr(object, env)?;
                    let method = self.eval_expr(property, env)?.display();
                    match target {
                        Value::Host(name) => return self.call_host(&name, &method, &values),
                        Value::String(text) => return string_method(&text, &method, &values),
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
                Ok(self.host.as_ref().map_or(Value::Undefined, |host| host.get(name, property)))
            }
            Value::Object(object) => {
                Ok(object.borrow().properties.get(property).cloned().unwrap_or(Value::Undefined))
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
                let property = self.eval_expr(property, env)?.display();
                match object {
                    Value::Host(name) => match self.host.as_mut() {
                        Some(host) => host.set(&name, &property, value),
                        None => Ok(()),
                    },
                    Value::Object(object) => {
                        object.borrow_mut().properties.insert(property, value);
                        Ok(())
                    }
                    _ => Ok(()),
                }
            }
            _ => Err(JsError { message: "invalid assignment target".into() }),
        }
    }

    fn call_host(&mut self, host: &str, method: &str, args: &[Value]) -> Result<Value, JsError> {
        match self.host.as_mut() {
            Some(bridge) => bridge.call(host, method, args),
            None => Ok(Value::Undefined),
        }
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
        self.budget = budget;
        self.call_depth = 0;
        self.call_value(callee, args)
    }

    fn call_value(&mut self, callee: Value, args: Vec<Value>) -> Result<Value, JsError> {
        self.tick()?;
        match callee {
            Value::Native(function) => function(self, args),
            Value::Function(function) => {
                if self.call_depth >= MAX_CALL_DEPTH {
                    return Err(JsError { message: "maximum call depth exceeded".into() });
                }
                self.call_depth += 1;
                let mut env = Env::child(&function.env);
                for (index, param) in function.params.iter().enumerate() {
                    let value = args.get(index).cloned().unwrap_or(Value::Undefined);
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

    fn binary(&self, op: &BinaryOp, left: Value, right: Value) -> Result<Value, JsError> {
        let result = match op {
            BinaryOp::Add => match (&left, &right) {
                (Value::String(_), _) | (_, Value::String(_)) => {
                    Value::String(format!("{}{}", left.display(), right.display()))
                }
                _ => Value::Number(left.number() + right.number()),
            },
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
    let text = args.iter().map(Value::display).collect::<Vec<_>>().join(" ");
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

fn native_parse_int(_: &mut Runtime, args: Vec<Value>) -> Result<Value, JsError> {
    let value = args.first().map_or_else(String::new, |value| value.display());
    let value = value.trim();
    let end = value
        .char_indices()
        .find(|(_, character)| {
            !character.is_ascii_digit() && *character != '-' && *character != '+'
        })
        .map_or(value.len(), |(index, _)| index);
    Ok(Value::Number(value[..end].parse::<f64>().unwrap_or(f64::NAN).trunc()))
}

fn native_parse_float(_: &mut Runtime, args: Vec<Value>) -> Result<Value, JsError> {
    let value = args.first().map_or_else(String::new, |value| value.display());
    Ok(Value::Number(value.trim().parse::<f64>().unwrap_or(f64::NAN)))
}

fn native_is_nan(_: &mut Runtime, args: Vec<Value>) -> Result<Value, JsError> {
    Ok(Value::Bool(args.first().is_none_or(|value| value.number().is_nan())))
}

fn string_method(value: &str, method: &str, args: &[Value]) -> Result<Value, JsError> {
    let result = match method {
        "toUpperCase" => Value::String(value.to_uppercase()),
        "toLowerCase" => Value::String(value.to_lowercase()),
        "trim" => Value::String(value.trim().to_owned()),
        "charAt" => {
            let index = args.first().map_or(0.0, Value::number).max(0.0) as usize;
            Value::String(value.chars().nth(index).map_or(String::new(), |ch| ch.to_string()))
        }
        "indexOf" => {
            let needle = args.first().map_or_else(String::new, |value| value.display());
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
            let value = args.first().cloned().unwrap_or(Value::Undefined);
            let length = {
                let mut object = array.borrow_mut();
                let length = object.properties.get("length").map_or(0.0, Value::number) as usize;
                object.properties.insert(length.to_string(), value);
                object.properties.insert("length".into(), Value::Number((length + 1) as f64));
                length
            };
            Ok(Value::Number((length + 1) as f64))
        }
        "join" => {
            let separator = args.first().map_or(String::new(), |value| value.display());
            let length =
                array.borrow().properties.get("length").map_or(0.0, Value::number) as usize;
            let values = (0..length)
                .map(|index| {
                    array
                        .borrow()
                        .properties
                        .get(&index.to_string())
                        .map_or(String::new(), Value::display)
                })
                .collect::<Vec<_>>();
            Ok(Value::String(values.join(&separator)))
        }
        "forEach" | "map" => {
            let callback = args.first().cloned().unwrap_or(Value::Undefined);
            let length =
                array.borrow().properties.get("length").map_or(0.0, Value::number) as usize;
            let output = Rc::new(RefCell::new(Object::default()));
            output.borrow_mut().is_array = true;
            output.borrow_mut().properties.insert("length".into(), Value::Number(length as f64));
            for index in 0..length {
                let item = array
                    .borrow()
                    .properties
                    .get(&index.to_string())
                    .cloned()
                    .unwrap_or(Value::Undefined);
                let result = runtime.call_value(
                    callback.clone(),
                    vec![item, Value::Number(index as f64), Value::Object(array.clone())],
                )?;
                if method == "map" {
                    output.borrow_mut().properties.insert(index.to_string(), result);
                }
            }
            if method == "map" { Ok(Value::Object(output)) } else { Ok(Value::Undefined) }
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
