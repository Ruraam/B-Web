//! DOM host bridge and event dispatch for the Rust JavaScript runtime.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::{Rc, Weak},
};

use bweb_html::{DomMutationError, DomReadError, HtmlDocument, NodeId, RemoveOutcome};
use bweb_js::{Host, JsError, MAX_ARRAY_LENGTH, MAX_STRING_BYTES, Object, Value};

/// Converts a host argument to text after checking the single-string limit.
/// The runtime reserves the cumulative budget for this conversion before the
/// bridge is invoked; this check prevents an unbounded allocation here.
fn host_display(value: &Value) -> Result<String, JsError> {
    value.display_limited(MAX_STRING_BYTES)
}

fn finish_budgeted_read(
    work_cell: &Rc<Cell<usize>>,
    remaining: usize,
    outcome: Result<Option<String>, DomReadError>,
) -> Result<String, JsError> {
    work_cell.set(remaining);
    match outcome {
        Ok(Some(text)) => Ok(text),
        Ok(None) | Err(DomReadError::TooLarge) => Ok(String::new()),
        Err(DomReadError::BudgetExhausted) => {
            Err(JsError { message: "DOM work budget exhausted".into() })
        }
        Err(DomReadError::ReservationFailed) => {
            Err(JsError { message: "script memory budget exceeded".into() })
        }
        Err(DomReadError::InvalidNode) => Err(JsError { message: "invalid element handle".into() }),
    }
}

#[derive(Default)]
pub struct EventRegistry {
    handlers: HashMap<(String, String), Vec<Value>>,
    prevented: bool,
}

impl EventRegistry {
    pub fn add(&mut self, object: &str, event: &str, callback: Value) {
        self.handlers.entry((object.to_owned(), event.to_owned())).or_default().push(callback);
    }

    #[must_use]
    pub fn callbacks(&self, object: &str, event: &str) -> Vec<Value> {
        self.handlers.get(&(object.to_owned(), event.to_owned())).cloned().unwrap_or_default()
    }

    pub fn reset(&mut self) {
        self.prevented = false;
    }

    pub fn clear(&mut self) {
        self.handlers.clear();
        self.prevented = false;
    }

    pub fn prevent_default(&mut self) {
        self.prevented = true;
    }

    #[must_use]
    pub const fn prevented(&self) -> bool {
        self.prevented
    }
}

/// Maximum number of DOM nodes a document's scripts may examine before the
/// bridge stops answering selector queries for that document.
pub const DOM_WORK_BUDGET: usize = 1_000_000;
/// Fixed accounting cost of one materialised host string.
const HOST_STRING_OVERHEAD: usize = 24;

fn decimal_digits(mut value: usize) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

fn handle_len(prefix: &str, index: usize) -> usize {
    prefix.len().saturating_add(decimal_digits(index))
}

/// Reserves the cost of a `Value::Host` handle before it is formatted.
fn reserve_handle(
    reserve: &mut dyn FnMut(usize) -> bool,
    prefix: &str,
    index: usize,
) -> Result<(), JsError> {
    if reserve(handle_len(prefix, index).saturating_add(HOST_STRING_OVERHEAD)) {
        Ok(())
    } else {
        Err(JsError { message: "script memory budget exceeded".into() })
    }
}

fn read_error(error: DomReadError) -> JsError {
    let message = match error {
        DomReadError::BudgetExhausted => "DOM work budget exhausted",
        DomReadError::ReservationFailed => "script memory budget exceeded",
        DomReadError::InvalidNode => "invalid element handle",
        DomReadError::TooLarge => "maximum string length exceeded",
    };
    JsError { message: message.into() }
}

fn mutation_error(error: DomMutationError) -> JsError {
    let message = match error {
        DomMutationError::InvalidNode => "invalid element handle",
        DomMutationError::InvalidChild => "node is not a child of this element",
        DomMutationError::AlreadyAttached => "node is already attached",
        DomMutationError::Cycle => "DOM cycle detected",
        DomMutationError::BudgetExhausted => "DOM work budget exhausted",
        DomMutationError::LimitExceeded => "DOM structure limit exceeded",
    };
    JsError { message: message.into() }
}

fn valid_class_token(token: &str) -> bool {
    !token.is_empty() && !token.chars().any(char::is_whitespace)
}

fn valid_attribute_name(name: &str) -> bool {
    !name.is_empty() && !name.chars().any(char::is_whitespace)
}

fn apply_class_tokens(
    document: &mut HtmlDocument,
    id: NodeId,
    tokens: &[String],
    work: &mut usize,
    reserve: &mut dyn FnMut(usize) -> bool,
) -> Result<(), JsError> {
    let total = tokens
        .iter()
        .map(String::len)
        .sum::<usize>()
        .saturating_add(tokens.len().saturating_sub(1));
    if !reserve(total.saturating_add(HOST_STRING_OVERHEAD)) {
        return Err(JsError { message: "script memory budget exceeded".into() });
    }
    let mut text = String::with_capacity(total);
    for (index, token) in tokens.iter().enumerate() {
        if index > 0 {
            text.push(' ');
        }
        text.push_str(token);
    }
    document.set_class_prereserved(id, text, work, reserve).map_err(read_error)
}

fn value_truthy(value: &Value) -> bool {
    match value {
        Value::Undefined | Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => *value != 0.0 && !value.is_nan(),
        Value::String(value) => !value.is_empty(),
        _ => true,
    }
}

fn dataset_attribute(key: &str, reserve: &mut dyn FnMut(usize) -> bool) -> Result<String, JsError> {
    if key.is_empty() || key.contains('-') || key.chars().any(char::is_whitespace) {
        return Err(JsError { message: "invalid dataset key".into() });
    }
    let mut length = "data-".len();
    for (index, character) in key.chars().enumerate() {
        length +=
            if character.is_ascii_uppercase() && index > 0 { 2 } else { character.len_utf8() };
    }
    if !reserve(length.saturating_add(HOST_STRING_OVERHEAD)) {
        return Err(JsError { message: "script memory budget exceeded".into() });
    }
    let mut name = String::with_capacity(length);
    name.push_str("data-");
    for (index, character) in key.chars().enumerate() {
        if character.is_ascii_uppercase() {
            if index > 0 {
                name.push('-');
            }
            name.push(character.to_ascii_lowercase());
        } else {
            name.push(character);
        }
    }
    Ok(name)
}

fn dataset_key(name: &str, reserve: &mut dyn FnMut(usize) -> bool) -> Result<String, JsError> {
    let Some(rest) = name.strip_prefix("data-") else {
        return Err(JsError { message: "invalid dataset attribute".into() });
    };
    if rest.is_empty() {
        return Err(JsError { message: "invalid dataset attribute".into() });
    }
    if !reserve(rest.len().saturating_add(HOST_STRING_OVERHEAD)) {
        return Err(JsError { message: "script memory budget exceeded".into() });
    }
    let mut key = String::with_capacity(rest.len());
    let mut uppercase = false;
    for character in rest.chars() {
        if character == '-' {
            uppercase = true;
        } else if uppercase {
            for upper in character.to_uppercase() {
                key.push(upper);
            }
            uppercase = false;
        } else {
            key.push(character);
        }
    }
    Ok(key)
}

fn argument_node(document: &HtmlDocument, value: &Value) -> Result<NodeId, JsError> {
    let handle =
        host_handle(value).ok_or_else(|| JsError { message: "invalid element handle".into() })?;
    let index = handle_index(&handle, "element:")
        .ok_or_else(|| JsError { message: "invalid element handle".into() })?;
    document.node_at(index).ok_or_else(|| JsError { message: "invalid element handle".into() })
}

/// Exposes a document to scripts through the `document` host object.
pub struct DomHost {
    document: Weak<RefCell<HtmlDocument>>,
    events: Rc<RefCell<EventRegistry>>,
    /// Shared by every script and callback of one document.
    work: Rc<Cell<usize>>,
}

impl DomHost {
    #[must_use]
    pub fn new(document: &Rc<RefCell<HtmlDocument>>, events: Rc<RefCell<EventRegistry>>) -> Self {
        Self {
            document: Rc::downgrade(document),
            events,
            work: Rc::new(Cell::new(DOM_WORK_BUDGET)),
        }
    }

    #[cfg(test)]
    pub(crate) fn set_work_budget(&self, budget: usize) {
        self.work.set(budget);
    }

    #[cfg(test)]
    pub(crate) fn work_cell(&self) -> Rc<Cell<usize>> {
        self.work.clone()
    }
}

fn handle_index(handle: &str, prefix: &str) -> Option<usize> {
    handle.strip_prefix(prefix)?.parse().ok()
}

fn host_handle(value: &Value) -> Option<String> {
    match value {
        Value::Host(name) => Some(name.clone()),
        _ => None,
    }
}

fn style_value_ref<'a>(style: &'a str, property: &str) -> Option<&'a str> {
    style.split(';').find_map(|declaration| {
        let (name, value) = declaration.split_once(':')?;
        (name.trim() == property).then(|| value.trim())
    })
}

fn style_value_bounded(style: &str, property: &str, max_bytes: usize) -> Option<String> {
    style_value_ref(style, property).filter(|value| value.len() <= max_bytes).map(str::to_owned)
}

fn with_style(
    document: &mut HtmlDocument,
    index: usize,
    property: &str,
    value: Option<&str>,
) -> Result<(), JsError> {
    let Some(id) = document.node_at(index) else { return Ok(()) };
    let style = document.attribute(id, "style").unwrap_or("");

    // Predict the final size before building any owned declaration strings.
    let mut predicted = 0usize;
    let mut declarations = 0usize;
    for part in style.split(';') {
        let Some((name, value)) = part.split_once(':') else { continue };
        let name = name.trim();
        if name == property {
            continue;
        }
        predicted = predicted
            .saturating_add(name.len())
            .saturating_add(2)
            .saturating_add(value.trim().len());
        declarations += 1;
    }
    if let Some(value) = value {
        predicted =
            predicted.saturating_add(property.len()).saturating_add(2).saturating_add(value.len());
        declarations += 1;
    }
    if declarations > 1 {
        predicted = predicted.saturating_add((declarations - 1).saturating_mul(2));
    }
    if predicted > MAX_STRING_BYTES {
        return Err(JsError { message: "maximum string length exceeded".into() });
    }

    let mut text = String::with_capacity(predicted);
    let mut first = true;
    for part in style.split(';') {
        let Some((name, value)) = part.split_once(':') else { continue };
        let name = name.trim();
        if name == property {
            continue;
        }
        if !first {
            text.push_str("; ");
        }
        text.push_str(name);
        text.push_str(": ");
        text.push_str(value.trim());
        first = false;
    }
    if let Some(value) = value {
        if !first {
            text.push_str("; ");
        }
        text.push_str(property);
        text.push_str(": ");
        text.push_str(value);
    }
    document.set_attribute(id, "style", &text);
    Ok(())
}

impl Host for DomHost {
    fn get(&self, object: &str, property: &str) -> Value {
        let Some(document) = self.document.upgrade() else { return Value::Undefined };
        let document = document.borrow();
        if object == "document" {
            return match property {
                "title" => {
                    Value::String(document.title_bounded(MAX_STRING_BYTES).unwrap_or_default())
                }
                _ => Value::Undefined,
            };
        }
        if let Some(index) = handle_index(object, "element:") {
            let Some(id) = document.node_at(index) else { return Value::Undefined };
            return match property {
                "style" => Value::Host(format!("style:{index}")),
                "textContent" => Value::String(
                    document.text_content_bounded(id, MAX_STRING_BYTES).unwrap_or_default(),
                ),
                "classList" => Value::Host(format!("classlist:{index}")),
                "id" | "class" | "href" | "src" => {
                    let value = document.attribute(id, property).unwrap_or("");
                    if value.len() > MAX_STRING_BYTES {
                        Value::Undefined
                    } else {
                        Value::String(value.to_owned())
                    }
                }
                _ => Value::Undefined,
            };
        }
        if let Some(index) = handle_index(object, "style:") {
            if let Some(id) = document.node_at(index) {
                let style = document.attribute(id, "style").unwrap_or("");
                return style_value_bounded(style, property, MAX_STRING_BYTES)
                    .map_or(Value::Undefined, Value::String);
            }
        }
        if let Some(index) = handle_index(object, "classlist:") {
            if let Some(id) = document.node_at(index) {
                return match property {
                    "length" => Value::Number(
                        document
                            .attribute(id, "class")
                            .unwrap_or("")
                            .split_ascii_whitespace()
                            .count() as f64,
                    ),
                    _ => Value::Bool(document.has_class(id, property)),
                };
            }
        }
        Value::Undefined
    }

    fn get_with_budget(
        &self,
        object: &str,
        property: &str,
        reserve: &mut dyn FnMut(usize) -> bool,
    ) -> Result<Option<Value>, JsError> {
        let Some(document) = self.document.upgrade() else { return Ok(Some(Value::Undefined)) };
        let document = document.borrow();
        if object == "document" && (property == "body" || property == "documentElement") {
            let tag = if property == "documentElement" { "html" } else { "body" };
            let mut work = self.work.get();
            let found = document.element_by_tag_budgeted(tag, &mut work);
            self.work.set(work);
            let found =
                found.map_err(|_| JsError { message: "DOM work budget exhausted".into() })?;
            let Some(id) = found else {
                return Ok(Some(Value::Undefined));
            };
            reserve_handle(reserve, "element:", id.index())?;
            return Ok(Some(Value::Host(format!("element:{}", id.index()))));
        }
        if object == "document" && property == "title" {
            let mut work = self.work.get();
            let outcome = document.title_budgeted(MAX_STRING_BYTES, &mut work, reserve);
            let title = finish_budgeted_read(&self.work, work, outcome)?;
            return Ok(Some(Value::String(title)));
        }
        if let Some(index) = handle_index(object, "element:") {
            let Some(id) = document.node_at(index) else { return Ok(Some(Value::Undefined)) };
            match property {
                "style" => {
                    reserve_handle(reserve, "style:", index)?;
                    return Ok(Some(Value::Host(format!("style:{index}"))));
                }
                "classList" => {
                    reserve_handle(reserve, "classlist:", index)?;
                    return Ok(Some(Value::Host(format!("classlist:{index}"))));
                }
                "textContent" => {
                    let mut work = self.work.get();
                    let outcome =
                        document.text_content_budgeted(id, MAX_STRING_BYTES, &mut work, reserve);
                    let text = finish_budgeted_read(&self.work, work, outcome.map(Some))?;
                    return Ok(Some(Value::String(text)));
                }
                "dataset" => {
                    reserve_handle(reserve, "dataset:", index)?;
                    return Ok(Some(Value::Host(format!("dataset:{index}"))));
                }
                "id" | "class" | "href" | "src" => {
                    let value = document.attribute(id, property).unwrap_or("");
                    if value.len() > MAX_STRING_BYTES
                        || !reserve(value.len() + HOST_STRING_OVERHEAD)
                    {
                        return Ok(Some(Value::Undefined));
                    }
                    return Ok(Some(Value::String(value.to_owned())));
                }
                _ => {}
            }
        }
        if let Some(index) = handle_index(object, "style:") {
            if let Some(id) = document.node_at(index) {
                let style = document.attribute(id, "style").unwrap_or("");
                if let Some(value) = style_value_ref(style, property) {
                    if value.len() > MAX_STRING_BYTES
                        || !reserve(value.len() + HOST_STRING_OVERHEAD)
                    {
                        return Ok(Some(Value::Undefined));
                    }
                    return Ok(Some(Value::String(value.to_owned())));
                }
            }
        }
        if let Some(index) = handle_index(object, "classlist:") {
            let Some(id) = document.node_at(index) else {
                return Err(JsError { message: "invalid element handle".into() });
            };
            let mut work = self.work.get();
            let outcome = document.class_tokens_budgeted(id, &mut work, reserve);
            self.work.set(work);
            let tokens = outcome.map_err(read_error)?;
            if property == "length" {
                return Ok(Some(Value::Number(tokens.len() as f64)));
            }
            if property == "value" {
                if !reserve(
                    tokens
                        .iter()
                        .map(String::len)
                        .sum::<usize>()
                        .saturating_add(tokens.len().saturating_sub(1))
                        .saturating_add(HOST_STRING_OVERHEAD),
                ) {
                    return Err(JsError { message: "script memory budget exceeded".into() });
                }
                return Ok(Some(Value::String(tokens.join(" "))));
            }
            if let Ok(position) = property.parse::<usize>() {
                if let Some(token) = tokens.get(position) {
                    if !reserve(token.len().saturating_add(HOST_STRING_OVERHEAD)) {
                        return Err(JsError { message: "script memory budget exceeded".into() });
                    }
                    return Ok(Some(Value::String(token.clone())));
                }
                return Ok(Some(Value::Undefined));
            }
            return Ok(Some(Value::Bool(tokens.iter().any(|token| token == property))));
        }
        if let Some(index) = handle_index(object, "dataset:") {
            let Some(id) = document.node_at(index) else {
                return Err(JsError { message: "invalid element handle".into() });
            };
            if property == "length" || property.parse::<usize>().is_ok() {
                let mut work = self.work.get();
                let outcome = document.attribute_names_budgeted(id, &mut work, reserve);
                self.work.set(work);
                let names = outcome.map_err(read_error)?;
                let keys: Vec<String> =
                    names.into_iter().filter(|name| name.starts_with("data-")).collect();
                if property == "length" {
                    return Ok(Some(Value::Number(keys.len() as f64)));
                }
                let position = property.parse::<usize>().unwrap_or(usize::MAX);
                if let Some(name) = keys.get(position) {
                    return Ok(Some(Value::String(dataset_key(name, reserve)?)));
                }
                return Ok(Some(Value::Undefined));
            }
            let name = dataset_attribute(property, reserve)?;
            let mut work = self.work.get();
            let outcome = document.attribute_budgeted(id, &name, &mut work, reserve);
            self.work.set(work);
            let value = outcome.map_err(read_error)?;
            return Ok(Some(value.map_or(Value::Undefined, Value::String)));
        }
        Ok(None)
    }

    fn set(&mut self, object: &str, property: &str, value: Value) -> Result<(), JsError> {
        let text = host_display(&value)?;
        let Some(document) = self.document.upgrade() else { return Ok(()) };
        let mut document = document.borrow_mut();
        if object == "document" && property == "title" {
            document.set_title(text);
            return Ok(());
        }
        if let Some(index) = handle_index(object, "element:") {
            let Some(id) = document.node_at(index) else { return Ok(()) };
            match property {
                "textContent" => document.set_text_content(id, &text),
                "id" | "class" | "href" | "src" | "style" => {
                    document.set_attribute(id, property, &text);
                }
                _ => {}
            }
            return Ok(());
        }
        if let Some(index) = handle_index(object, "style:") {
            with_style(&mut document, index, property, Some(&text))?;
        }
        Ok(())
    }

    fn set_with_budget(
        &mut self,
        object: &str,
        property: &str,
        value: Value,
        reserve: &mut dyn FnMut(usize) -> bool,
    ) -> Result<Option<()>, JsError> {
        let Some(index) = handle_index(object, "dataset:") else {
            return Ok(None);
        };
        let name = dataset_attribute(property, reserve)?;
        let hint = value.display_size_hint();
        if hint > MAX_STRING_BYTES || !reserve(hint.saturating_add(HOST_STRING_OVERHEAD)) {
            return Err(JsError { message: "script memory budget exceeded".into() });
        }
        let text = value.display_limited(MAX_STRING_BYTES)?;
        let Some(document) = self.document.upgrade() else {
            return Err(JsError { message: "invalid element handle".into() });
        };
        let mut document = document.borrow_mut();
        let Some(id) = document.node_at(index) else {
            return Err(JsError { message: "invalid element handle".into() });
        };
        let mut work = self.work.get();
        let outcome = document.set_attribute_prereserved(id, &name, text, &mut work, reserve);
        self.work.set(work);
        outcome.map_err(read_error)?;
        Ok(Some(()))
    }

    fn call(&mut self, object: &str, method: &str, args: &[Value]) -> Result<Value, JsError> {
        if let Some(index) = handle_index(object, "classlist:") {
            let class = match args.first() {
                Some(value) => host_display(value)?,
                None => String::new(),
            };
            let Some(document) = self.document.upgrade() else { return Ok(Value::Undefined) };
            let mut document = document.borrow_mut();
            let Some(id) = document.node_at(index) else { return Ok(Value::Undefined) };
            return match method {
                "add" => {
                    document.set_class(id, &class, true);
                    Ok(Value::Undefined)
                }
                "remove" => {
                    document.set_class(id, &class, false);
                    Ok(Value::Undefined)
                }
                "contains" => Ok(Value::Bool(document.has_class(id, &class))),
                "toggle" => {
                    let present = document.has_class(id, &class);
                    document.set_class(id, &class, !present);
                    Ok(Value::Bool(!present))
                }
                _ => Ok(Value::Undefined),
            };
        }
        if method == "addEventListener" {
            let event = match args.first() {
                Some(value) => host_display(value)?,
                None => String::new(),
            };
            if let Some(callback) = args.get(1).cloned() {
                self.events.borrow_mut().add(object, &event, callback);
            }
            return Ok(Value::Undefined);
        }
        if object == "event" && method == "preventDefault" {
            self.events.borrow_mut().prevent_default();
            return Ok(Value::Undefined);
        }
        if let Some(index) = handle_index(object, "element:") {
            let Some(document) = self.document.upgrade() else { return Ok(Value::Undefined) };
            let mut document = document.borrow_mut();
            if method == "remove" && document.node_at(index).is_none() {
                return Err(JsError { message: "invalid element handle".into() });
            }
            let Some(id) = document.node_at(index) else { return Ok(Value::Undefined) };
            return match method {
                "setAttribute" => {
                    if let Some(name) = args.first() {
                        let name = host_display(name)?;
                        let value = match args.get(1) {
                            Some(value) => host_display(value)?,
                            None => String::new(),
                        };
                        document.set_attribute(id, &name, &value);
                    }
                    Ok(Value::Undefined)
                }
                "getAttribute" => {
                    let name = match args.first() {
                        Some(value) => host_display(value)?,
                        None => String::new(),
                    };
                    Ok(document
                        .attribute(id, &name)
                        .map_or(Value::Null, |v| Value::String(v.to_owned())))
                }
                "appendChild" => {
                    let Some(child) = args.first() else { return Ok(Value::Null) };
                    let Some(handle) = host_handle(child) else { return Ok(Value::Null) };
                    let Some(child_index) = handle_index(&handle, "element:") else {
                        return Ok(Value::Null);
                    };
                    let Some(child_id) = document.node_at(child_index) else {
                        return Ok(Value::Null);
                    };
                    Ok(if document.append_child(id, child_id) {
                        child.clone()
                    } else {
                        Value::Null
                    })
                }
                "remove" => {
                    let mut work = self.work.get();
                    let outcome = document.remove_node_budgeted(id, &mut work);
                    self.work.set(work);
                    match outcome {
                        Ok(Some(RemoveOutcome::Removed))
                        | Ok(Some(RemoveOutcome::AlreadyDetached)) => Ok(Value::Undefined),
                        Ok(None) => {
                            Err(JsError { message: "cannot remove a top-level element".into() })
                        }
                        Err(DomReadError::BudgetExhausted) => {
                            Err(JsError { message: "DOM work budget exhausted".into() })
                        }
                        Err(DomReadError::InvalidNode) => {
                            Err(JsError { message: "invalid element handle".into() })
                        }
                        Err(_) => Err(JsError { message: "invalid element handle".into() }),
                    }
                }
                _ => Ok(Value::Undefined),
            };
        }
        if object == "document" {
            match method {
                "querySelectorAll" => {
                    let Some(selector) = args.first().map(host_display).transpose()? else {
                        return Ok(Value::Object(Rc::new(RefCell::new(Object {
                            properties: HashMap::from([("length".into(), Value::Number(0.0))]),
                            is_array: true,
                        }))));
                    };
                    let Some(document) = self.document.upgrade() else {
                        return Ok(Value::Undefined);
                    };
                    let document = document.borrow();
                    // Every examined node consumes the document's shared DOM
                    // work budget; an exhausted budget returns no results.
                    let mut work = self.work.get();
                    let nodes =
                        document.query_selector_all_checked(&selector, MAX_ARRAY_LENGTH, &mut work);
                    self.work.set(work);
                    let nodes = nodes
                        .map_err(|_| JsError { message: "DOM work budget exhausted".into() })?;
                    let visible = nodes.len();
                    let mut properties = HashMap::with_capacity(visible.saturating_add(1));
                    for (index, node) in nodes.iter().enumerate() {
                        properties.insert(
                            index.to_string(),
                            Value::Host(format!("element:{}", node.index())),
                        );
                    }
                    properties.insert("length".into(), Value::Number(visible as f64));
                    return Ok(Value::Object(Rc::new(RefCell::new(Object {
                        properties,
                        is_array: true,
                    }))));
                }
                "createElement" => {
                    let Some(tag) = args.first().map(host_display).transpose()? else {
                        return Ok(Value::Null);
                    };
                    let Some(document) = self.document.upgrade() else {
                        return Ok(Value::Undefined);
                    };
                    let mut document = document.borrow_mut();
                    if !document.can_create_node() {
                        return Err(JsError { message: "element budget exhausted".into() });
                    }
                    let node = document.create_element(&tag);
                    return Ok(Value::Host(format!("element:{}", node.index())));
                }
                "getElementById" | "querySelector" => {
                    let Some(selector) = args.first().map(host_display).transpose()? else {
                        return Ok(Value::Null);
                    };
                    let Some(document) = self.document.upgrade() else {
                        return Ok(Value::Undefined);
                    };
                    let document = document.borrow();
                    let mut work = self.work.get();
                    let found = if method == "getElementById" {
                        document.element_by_id_budgeted(&selector, &mut work)
                    } else {
                        document.query_selector_budgeted(&selector, &mut work)
                    };
                    self.work.set(work);
                    let found = found
                        .map_err(|_| JsError { message: "DOM work budget exhausted".into() })?;
                    return Ok(found.map_or(Value::Null, |node| {
                        Value::Host(format!("element:{}", node.index()))
                    }));
                }
                "write" => {
                    let mut parts = Vec::with_capacity(args.len());
                    let mut total = 0usize;
                    for argument in args {
                        let part = host_display(argument)?;
                        total = total.saturating_add(part.len());
                        parts.push(part);
                    }
                    if args.len() > 1 {
                        total = total.saturating_add(args.len() - 1);
                    }
                    if total > MAX_STRING_BYTES {
                        return Err(JsError { message: "maximum string length exceeded".into() });
                    }
                    let text = parts.join(" ");
                    let Some(document) = self.document.upgrade() else {
                        return Ok(Value::Undefined);
                    };
                    let mut document = document.borrow_mut();
                    let mut work = self.work.get();
                    let body = document.element_by_tag_budgeted("body", &mut work);
                    self.work.set(work);
                    let body =
                        body.map_err(|_| JsError { message: "DOM work budget exhausted".into() })?;
                    if let Some(body) = body {
                        document.set_text_content(body, &text);
                    }
                }
                _ => {}
            }
        }
        Ok(Value::Undefined)
    }

    fn call_with_budget(
        &mut self,
        object: &str,
        method: &str,
        args: &[Value],
        reserve: &mut dyn FnMut(usize) -> bool,
    ) -> Result<Option<Value>, JsError> {
        if let Some(index) = handle_index(object, "classlist:") {
            if matches!(method, "add" | "remove" | "toggle" | "replace" | "contains" | "item") {
                let Some(document) = self.document.upgrade() else {
                    return Err(JsError { message: "invalid element handle".into() });
                };
                let mut document = document.borrow_mut();
                let Some(id) = document.node_at(index) else {
                    return Err(JsError { message: "invalid element handle".into() });
                };
                let mut work = self.work.get();
                let outcome = document.class_tokens_budgeted(id, &mut work, reserve);
                let result: Result<Value, JsError> = match outcome {
                    Err(error) => Err(read_error(error)),
                    Ok(mut tokens) => {
                        for argument in args {
                            if let Value::String(text) = argument {
                                if !valid_class_token(text) {
                                    self.work.set(work);
                                    return Err(JsError { message: "invalid class token".into() });
                                }
                            }
                        }
                        if !reserve(
                            args.len()
                                .saturating_mul(std::mem::size_of::<String>())
                                .saturating_add(HOST_STRING_OVERHEAD),
                        ) {
                            self.work.set(work);
                            return Err(JsError {
                                message: "script memory budget exceeded".into(),
                            });
                        }
                        let mut arguments = Vec::with_capacity(args.len());
                        for argument in args {
                            let text = match host_display(argument) {
                                Ok(text) => text,
                                Err(error) => {
                                    self.work.set(work);
                                    return Err(error);
                                }
                            };
                            if !valid_class_token(&text) {
                                self.work.set(work);
                                return Err(JsError { message: "invalid class token".into() });
                            }
                            arguments.push(text);
                        }
                        let minimum = match method {
                            "toggle" => 1,
                            "replace" => 2,
                            _ => 0,
                        };
                        if arguments.len() < minimum {
                            self.work.set(work);
                            return Err(JsError { message: "invalid class token".into() });
                        }
                        match method {
                            "add" => {
                                if arguments.is_empty() {
                                    Ok(Value::Undefined)
                                } else {
                                    for token in &arguments {
                                        if !tokens.iter().any(|existing| existing == token) {
                                            tokens.push(token.clone());
                                        }
                                    }
                                    apply_class_tokens(
                                        &mut document,
                                        id,
                                        &tokens,
                                        &mut work,
                                        reserve,
                                    )
                                    .map(|()| Value::Undefined)
                                }
                            }
                            "remove" => {
                                if arguments.is_empty() {
                                    Ok(Value::Undefined)
                                } else {
                                    tokens
                                        .retain(|token| !arguments.iter().any(|arg| arg == token));
                                    apply_class_tokens(
                                        &mut document,
                                        id,
                                        &tokens,
                                        &mut work,
                                        reserve,
                                    )
                                    .map(|()| Value::Undefined)
                                }
                            }
                            "toggle" => {
                                let token = &arguments[0];
                                let enabled = args.get(1).is_none_or(value_truthy);
                                let present = tokens.iter().any(|existing| existing == token);
                                if enabled && !present {
                                    tokens.push(token.clone());
                                    apply_class_tokens(
                                        &mut document,
                                        id,
                                        &tokens,
                                        &mut work,
                                        reserve,
                                    )
                                    .map(|()| Value::Bool(enabled))
                                } else if !enabled && present {
                                    tokens.retain(|existing| existing != token);
                                    apply_class_tokens(
                                        &mut document,
                                        id,
                                        &tokens,
                                        &mut work,
                                        reserve,
                                    )
                                    .map(|()| Value::Bool(enabled))
                                } else {
                                    Ok(Value::Bool(enabled))
                                }
                            }
                            "replace" => {
                                let old = &arguments[0];
                                let new = &arguments[1];
                                if !tokens.iter().any(|existing| existing == old) {
                                    Ok(Value::Bool(false))
                                } else {
                                    for token in &mut tokens {
                                        if token == old {
                                            *token = new.clone();
                                        }
                                    }
                                    tokens.dedup();
                                    apply_class_tokens(
                                        &mut document,
                                        id,
                                        &tokens,
                                        &mut work,
                                        reserve,
                                    )
                                    .map(|()| Value::Bool(true))
                                }
                            }
                            "contains" => Ok(Value::Bool(
                                arguments
                                    .first()
                                    .is_some_and(|token| tokens.iter().any(|t| t == token)),
                            )),
                            "item" => {
                                let index =
                                    arguments.first().and_then(|value| value.parse::<usize>().ok());
                                Ok(index
                                    .and_then(|index| tokens.get(index).cloned())
                                    .map_or(Value::Undefined, Value::String))
                            }
                            _ => unreachable!("method was checked above"),
                        }
                    }
                };
                self.work.set(work);
                return result.map(Some);
            }
        }
        if let Some(index) = handle_index(object, "element:") {
            if matches!(
                method,
                "setAttribute" | "removeAttribute" | "hasAttribute" | "toggleAttribute"
            ) {
                let Some(document) = self.document.upgrade() else {
                    return Err(JsError { message: "invalid element handle".into() });
                };
                let mut document = document.borrow_mut();
                let Some(id) = document.node_at(index) else {
                    return Err(JsError { message: "invalid element handle".into() });
                };
                let mut work = self.work.get();
                let name = match args.first() {
                    Some(value) => match host_display(value) {
                        Ok(name) => name,
                        Err(error) => {
                            self.work.set(work);
                            return Err(error);
                        }
                    },
                    None => {
                        self.work.set(work);
                        return Err(JsError { message: "invalid attribute name".into() });
                    }
                };
                if !valid_attribute_name(&name) {
                    self.work.set(work);
                    return Err(JsError { message: "invalid attribute name".into() });
                }
                let result: Result<Value, JsError> = match method {
                    "setAttribute" => {
                        let hint = args.get(1).map_or(0, Value::display_size_hint);
                        if hint > MAX_STRING_BYTES
                            || !reserve(hint.saturating_add(HOST_STRING_OVERHEAD))
                        {
                            self.work.set(work);
                            return Err(JsError {
                                message: "script memory budget exceeded".into(),
                            });
                        }
                        let value = args.get(1).map_or(String::new(), |value| {
                            value.display_limited(MAX_STRING_BYTES).unwrap_or_default()
                        });
                        document
                            .set_attribute_prereserved(id, &name, value, &mut work, reserve)
                            .map(|()| Value::Undefined)
                            .map_err(read_error)
                    }
                    "removeAttribute" => document
                        .remove_attribute_budgeted(id, &name, &mut work)
                        .map(|_| Value::Undefined)
                        .map_err(read_error),
                    "hasAttribute" => document
                        .has_attribute_budgeted(id, &name, &mut work)
                        .map(Value::Bool)
                        .map_err(read_error),
                    "toggleAttribute" => {
                        let force = args.get(1).map(value_truthy);
                        document
                            .toggle_attribute_budgeted(id, &name, force, &mut work, reserve)
                            .map(Value::Bool)
                            .map_err(read_error)
                    }
                    _ => unreachable!("method was checked above"),
                };
                self.work.set(work);
                return result.map(Some);
            }
        }
        if let Some(index) = handle_index(object, "element:") {
            if matches!(method, "append" | "prepend" | "removeChild" | "replaceChild") {
                let Some(document) = self.document.upgrade() else {
                    return Err(JsError { message: "invalid element handle".into() });
                };
                let mut document = document.borrow_mut();
                let Some(parent) = document.node_at(index) else {
                    return Err(JsError { message: "invalid element handle".into() });
                };
                let mut work = self.work.get();
                let outcome = match method {
                    "append" | "prepend" => {
                        if !reserve(
                            args.len()
                                .saturating_mul(std::mem::size_of::<NodeId>())
                                .saturating_add(HOST_STRING_OVERHEAD),
                        ) {
                            return Err(JsError {
                                message: "script memory budget exceeded".into(),
                            });
                        }
                        let mut nodes = Vec::with_capacity(args.len());
                        let mut conversion = Ok(());
                        for arg in args {
                            match argument_node(&document, arg) {
                                Ok(node) => nodes.push(node),
                                Err(error) => {
                                    conversion = Err(error);
                                    break;
                                }
                            }
                        }
                        if let Err(error) = conversion {
                            self.work.set(work);
                            return Err(error);
                        }
                        let result = if method == "append" {
                            document.append_children_budgeted(parent, &nodes, &mut work)
                        } else {
                            document.prepend_children_budgeted(parent, &nodes, &mut work)
                        };
                        result.map(|()| Value::Undefined).map_err(mutation_error)
                    }
                    "removeChild" => {
                        let child = argument_node(
                            &document,
                            args.first().ok_or_else(|| JsError {
                                message: "invalid element handle".into(),
                            })?,
                        )?;
                        let plan = document.plan_remove_child_budgeted(parent, child, &mut work);
                        self.work.set(work);
                        let position = plan.map_err(mutation_error)?;
                        // Reserve only after the mutation is known to succeed.
                        reserve_handle(reserve, "element:", child.index())?;
                        document.apply_remove_child(parent, position);
                        Ok(Value::Host(format!("element:{}", child.index())))
                    }
                    "replaceChild" => {
                        let new_child = argument_node(
                            &document,
                            args.first().ok_or_else(|| JsError {
                                message: "invalid element handle".into(),
                            })?,
                        )?;
                        let old_child = argument_node(
                            &document,
                            args.get(1).ok_or_else(|| JsError {
                                message: "invalid element handle".into(),
                            })?,
                        )?;
                        let plan = document
                            .plan_replace_child_budgeted(parent, new_child, old_child, &mut work);
                        self.work.set(work);
                        let position = plan.map_err(mutation_error)?;
                        // Reserve only after the mutation is known to succeed.
                        reserve_handle(reserve, "element:", old_child.index())?;
                        document.apply_replace_child(parent, position, new_child);
                        Ok(Value::Host(format!("element:{}", old_child.index())))
                    }
                    _ => unreachable!("method was checked above"),
                };
                self.work.set(work);
                return outcome.map(Some);
            }
        }
        if method == "getAttribute" {
            if let Some(index) = handle_index(object, "element:") {
                let Some(name) = args.first() else { return Ok(Some(Value::Null)) };
                let name = name.display_limited(MAX_STRING_BYTES)?;
                let Some(document) = self.document.upgrade() else { return Ok(Some(Value::Null)) };
                let document = document.borrow();
                let Some(id) = document.node_at(index) else { return Ok(Some(Value::Null)) };
                if let Some(value) = document.attribute(id, &name) {
                    if value.len() > MAX_STRING_BYTES
                        || !reserve(value.len() + HOST_STRING_OVERHEAD)
                    {
                        return Ok(Some(Value::Null));
                    }
                    return Ok(Some(Value::String(value.to_owned())));
                }
                return Ok(Some(Value::Null));
            }
        }
        if object == "document" && (method == "querySelector" || method == "getElementById") {
            let selector = match args.first() {
                Some(value) => value.display_limited(MAX_STRING_BYTES)?,
                None => String::new(),
            };
            let Some(document) = self.document.upgrade() else { return Ok(Some(Value::Null)) };
            let document = document.borrow();
            let mut work = self.work.get();
            let found = if method == "getElementById" {
                document.element_by_id_budgeted(&selector, &mut work)
            } else {
                document.query_selector_budgeted(&selector, &mut work)
            };
            self.work.set(work);
            let found =
                found.map_err(|_| JsError { message: "DOM work budget exhausted".into() })?;
            let Some(node) = found else {
                return Ok(Some(Value::Null));
            };
            reserve_handle(reserve, "element:", node.index())?;
            return Ok(Some(Value::Host(format!("element:{}", node.index()))));
        }
        if object == "document" && method == "querySelectorAll" {
            let selector = match args.first() {
                Some(value) => value.display_limited(MAX_STRING_BYTES)?,
                None => String::new(),
            };
            let Some(document) = self.document.upgrade() else { return Ok(Some(Value::Undefined)) };
            let document = document.borrow();
            let mut work = self.work.get();
            let nodes = document.query_selector_all_checked(&selector, MAX_ARRAY_LENGTH, &mut work);
            self.work.set(work);
            let nodes =
                nodes.map_err(|_| JsError { message: "DOM work budget exhausted".into() })?;
            // Charge every allocation the handle array will perform: its
            // containers, the "length" key, each index key and each
            // "element:<index>" handle string.
            let mut cost = HOST_STRING_OVERHEAD
                .saturating_mul(2)
                .saturating_add(HOST_STRING_OVERHEAD)
                .saturating_add("length".len());
            for (index, node) in nodes.iter().enumerate() {
                cost = cost
                    .saturating_add(HOST_STRING_OVERHEAD)
                    .saturating_add(decimal_digits(index).saturating_add(HOST_STRING_OVERHEAD))
                    .saturating_add(
                        handle_len("element:", node.index()).saturating_add(HOST_STRING_OVERHEAD),
                    );
            }
            if !reserve(cost) {
                return Err(JsError { message: "script memory budget exceeded".into() });
            }
            let mut properties = HashMap::with_capacity(nodes.len().saturating_add(1));
            for (index, node) in nodes.iter().enumerate() {
                properties
                    .insert(index.to_string(), Value::Host(format!("element:{}", node.index())));
            }
            properties.insert("length".into(), Value::Number(nodes.len() as f64));
            return Ok(Some(Value::Object(Rc::new(RefCell::new(Object {
                properties,
                is_array: true,
            })))));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::{DomHost, EventRegistry};
    use bweb_html::{HtmlDocument, NodeKind};
    use bweb_js::{JsError, MAX_ARRAY_LENGTH, MAX_STRING_BYTES, Runtime, Value, parse};
    use std::{cell::RefCell, rc::Rc};

    fn eval_with_budget(
        document: &Rc<RefCell<HtmlDocument>>,
        source: &str,
        budget: usize,
    ) -> Result<(String, usize), JsError> {
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(DomHost::new(
            document,
            Rc::new(RefCell::new(EventRegistry::default())),
        )));
        runtime.define_global("document", Value::Host("document".into()));
        runtime.set_memory_budget(budget);
        let value = runtime.eval(source)?;
        Ok((value.display(), runtime.memory_used()))
    }

    #[test]
    fn host_survives_as_long_as_the_document_rc_is_alive() {
        let document = Rc::new(RefCell::new(HtmlDocument::parse(
            "<html><body><p id='x'>one</p></body></html>",
        )));
        let events = Rc::new(RefCell::new(EventRegistry::default()));
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(DomHost::new(&document, events.clone())));
        runtime.define_global("document", Value::Host("document".into()));
        // Register a handler, then keep the Rc alive (the real fix in the shell).
        runtime
            .eval("document.getElementById('x').addEventListener('click', function(){ document.title = 'clicked'; });")
            .expect("script");
        assert_eq!(document.borrow().title(), None);
        // Simulate an event callback firing later, while the Rc is still alive.
        let handle = {
            let dom = document.borrow();
            let id = dom.element_by_id("x").expect("paragraph");
            format!("element:{}", id.index())
        };
        let callbacks = events.borrow().callbacks(&handle, "click");
        for callback in callbacks {
            runtime.call(callback, vec![Value::Host("event".into())]).expect("handler");
        }
        assert_eq!(document.borrow().title().as_deref(), Some("clicked"));
    }

    #[test]
    fn scripts_can_mutate_the_document() {
        let document = Rc::new(RefCell::new(HtmlDocument::parse(
            "<html><body><p id='target'>before</p></body></html>",
        )));
        let events = Rc::new(RefCell::new(EventRegistry::default()));
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(DomHost::new(&document, events)));
        runtime.define_global("document", Value::Host("document".into()));
        runtime
            .eval("document.title = 'New'; let p = document.getElementById('target'); p.style.color = 'red'; p.textContent = 'after';")
            .expect("script");
        let borrowed = document.borrow();
        assert_eq!(borrowed.title().as_deref(), Some("New"));
        let target = borrowed.element_by_id("target").expect("target");
        assert_eq!(borrowed.text_content(target), "after");
        assert_eq!(borrowed.attribute(target, "style"), Some("color: red"));
    }

    #[test]
    fn query_selector_all_and_class_list_work() {
        let document = Rc::new(RefCell::new(HtmlDocument::parse(
            "<main><p class='a first'>1</p><p class='a'>2</p><span>3</span></main>",
        )));
        let events = Rc::new(RefCell::new(EventRegistry::default()));
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(DomHost::new(&document, events.clone())));
        runtime.define_global("document", Value::Host("document".into()));
        let value = runtime
            .eval("document.querySelectorAll('p').length + ':' + document.querySelector('p').classList.contains('first')")
            .expect("script");
        assert_eq!(value.display(), "2:true");

        runtime
            .eval("let p = document.querySelector('span'); p.classList.add('a'); p.classList.toggle('hot'); p.setAttribute('id', 'x');")
            .expect("script");
        let dom = document.borrow();
        let span = dom
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), bweb_html::NodeKind::Element { tag_name, .. } if tag_name == "span")
                    .then_some(id)
            })
            .expect("span");
        assert!(dom.has_class(span, "a"));
        assert!(dom.has_class(span, "hot"));
        assert_eq!(dom.attribute(span, "id"), Some("x"));
    }

    #[test]
    fn create_element_and_append_child_mutate_the_dom() {
        let document = Rc::new(RefCell::new(HtmlDocument::parse("<main>hi</main>")));
        let events = Rc::new(RefCell::new(EventRegistry::default()));
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(DomHost::new(&document, events.clone())));
        runtime.define_global("document", Value::Host("document".into()));
        runtime
            .eval("let el = document.createElement('em'); el.textContent = 'new'; document.querySelector('main').appendChild(el);")
            .expect("script");
        let dom = document.borrow();
        let em = dom
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), bweb_html::NodeKind::Element { tag_name, .. } if tag_name == "em")
                    .then_some(id)
            })
            .expect("em");
        assert_eq!(dom.text_content(em), "new");
        assert!(dom.ancestor_elements(em).iter().any(|id| {
            matches!(dom.node(*id).map(|node| node.kind()), Some(bweb_html::NodeKind::Element { tag_name, .. }) if tag_name == "main")
        }));
    }

    #[test]
    fn todo_list_pattern_builds_and_styles_items() {
        let document = Rc::new(RefCell::new(HtmlDocument::parse(
            "<html><body><ul id='todo'></ul></body></html>",
        )));
        let events = Rc::new(RefCell::new(EventRegistry::default()));
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(DomHost::new(&document, events.clone())));
        runtime.define_global("document", Value::Host("document".into()));
        // A typical "render a list from data" snippet using only supported DOM APIs.
        runtime
            .eval(
                "var list = document.getElementById('todo'); \
                 var labels = ['Buy milk', 'Write tests', 'Ship it']; \
                 for (var i = 0; i < labels.length; i = i + 1) { \
                   var li = document.createElement('li'); \
                   li.textContent = labels[i]; \
                   li.classList.add('task'); \
                   if (i == 0) { li.classList.add('first'); } \
                   list.appendChild(li); \
                 }",
            )
            .expect("script");
        let dom = document.borrow();
        let items = dom.query_selector_all("li");
        assert_eq!(items.len(), 3);
        assert_eq!(dom.text_content(items[0]), "Buy milk");
        assert_eq!(dom.text_content(items[2]), "Ship it");
        assert!(dom.has_class(items[0], "first"));
        assert!(dom.has_class(items[2], "task"));
        let list = dom.element_by_id("todo").expect("list");
        assert!(dom.ancestor_elements(items[0]).contains(&list));
    }

    #[test]
    fn query_selector_all_for_each_updates_every_item() {
        let document = Rc::new(RefCell::new(HtmlDocument::parse(
            "<html><body><ul>\
               <li class='task'>a</li><li class='task'>b</li><li class='task'>c</li>\
             </ul></body></html>",
        )));
        let events = Rc::new(RefCell::new(EventRegistry::default()));
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(DomHost::new(&document, events.clone())));
        runtime.define_global("document", Value::Host("document".into()));
        let value = runtime
            .eval(
                "var done = 0; \
                 document.querySelectorAll('.task').forEach(function(item) { \
                   item.classList.add('done'); \
                   done = done + 1; \
                 }); \
                 done",
            )
            .expect("script");
        assert_eq!(value.display(), "3");
        let dom = document.borrow();
        for item in dom.query_selector_all(".task") {
            assert!(dom.has_class(item, "done"));
        }
    }

    #[test]
    fn click_handler_mutates_dom_through_the_bridge() {
        let document = Rc::new(RefCell::new(HtmlDocument::parse(
            "<html><body>\
               <button id='toggle'>Toggle</button><p id='status'>off</p>\
             </body></html>",
        )));
        let events = Rc::new(RefCell::new(EventRegistry::default()));
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(DomHost::new(&document, events.clone())));
        runtime.define_global("document", Value::Host("document".into()));
        runtime
            .eval(
                "document.getElementById('toggle').addEventListener('click', function() { \
                   var status = document.getElementById('status'); \
                   status.textContent = 'on'; \
                   status.classList.add('active'); \
                 });",
            )
            .expect("script");
        let handle = {
            let dom = document.borrow();
            format!("element:{}", dom.element_by_id("toggle").expect("button").index())
        };
        let callbacks = events.borrow().callbacks(&handle, "click");
        assert_eq!(callbacks.len(), 1, "one click handler registered");
        for callback in callbacks {
            runtime.call(callback, vec![Value::Host("event".into())]).expect("handler");
        }
        let dom = document.borrow();
        let status = dom.element_by_id("status").expect("status");
        assert_eq!(dom.text_content(status), "on");
        assert!(dom.has_class(status, "active"));
    }

    #[test]
    fn event_listeners_are_registered() {
        let document = Rc::new(RefCell::new(HtmlDocument::parse("<p>x</p>")));
        let events = Rc::new(RefCell::new(EventRegistry::default()));
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(DomHost::new(&document, events.clone())));
        runtime.define_global("document", Value::Host("document".into()));
        runtime
            .eval("document.querySelector('p').addEventListener('click', function(e) { e.preventDefault(); });")
            .expect("script");
        let paragraph = document
            .borrow()
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "p")
                    .then_some(id)
            })
            .expect("paragraph");
        assert_eq!(
            events.borrow().callbacks(&format!("element:{}", paragraph.index()), "click").len(),
            1
        );
    }

    #[test]
    fn query_selector_all_is_capped_to_the_array_limit() {
        let document =
            Rc::new(RefCell::new(HtmlDocument::parse("<i></i>".repeat(MAX_ARRAY_LENGTH + 1))));
        let events = Rc::new(RefCell::new(EventRegistry::default()));
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(DomHost::new(&document, events)));
        runtime.define_global("document", Value::Host("document".into()));
        let length = runtime
            .eval("document.querySelectorAll('i').length")
            .expect("querySelectorAll must stay within the array limit");
        assert_eq!(length.display(), MAX_ARRAY_LENGTH.to_string());
    }

    #[test]
    fn dom_argument_conversion_rejects_oversized_strings() {
        let value = Value::String("x".repeat(MAX_STRING_BYTES + 1));
        let error = super::host_display(&value).expect_err("conversion must be refused");
        assert!(error.message.contains("string length"), "unexpected error: {}", error.message);
    }

    #[test]
    fn dom_work_budget_is_shared_and_stops_repeated_selectors() {
        let document = Rc::new(RefCell::new(HtmlDocument::parse("<span></span>".repeat(100))));
        let events = Rc::new(RefCell::new(EventRegistry::default()));
        let host = DomHost::new(&document, events);
        host.set_work_budget(10);
        let work = host.work_cell();
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(host));
        runtime.define_global("document", Value::Host("document".into()));

        // A selector with no matches still examines nodes; exhaustion is now
        // reported as an error instead of an empty result.
        let error = runtime
            .eval("document.querySelectorAll('missing').length")
            .expect_err("first query must report exhaustion");
        assert!(error.message.contains("DOM work budget"), "unexpected error: {}", error.message);
        assert_eq!(work.get(), 0);
        let error = runtime
            .eval("document.querySelectorAll('span').length")
            .expect_err("second query must also be refused");
        assert!(error.message.contains("DOM work budget"), "unexpected error: {}", error.message);
    }

    #[test]
    fn oversized_dom_reads_are_rejected_before_returning_strings() {
        let document = Rc::new(RefCell::new(HtmlDocument::parse("<p id='x'>x</p>")));
        let oversized = "x".repeat(MAX_STRING_BYTES + 1);
        {
            let mut dom = document.borrow_mut();
            let id = dom.element_by_id("x").expect("paragraph");
            dom.set_text_content(id, &oversized);
            dom.set_title(&oversized);
            dom.set_attribute(id, "style", &format!("color: {oversized}"));
        }
        let events = Rc::new(RefCell::new(EventRegistry::default()));
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(DomHost::new(&document, events)));
        runtime.define_global("document", Value::Host("document".into()));
        assert_eq!(runtime.eval("document.title").expect("title").display(), "");
        assert_eq!(
            runtime
                .eval("document.getElementById('x').textContent")
                .expect("textContent")
                .display(),
            ""
        );
        assert_eq!(
            runtime
                .eval("document.getElementById('x').style.color")
                .expect("style value")
                .display(),
            "undefined"
        );
        assert!(runtime.memory_used() < 5000, "oversized DOM text must not be charged as retained");
    }

    #[test]
    fn oversized_get_attribute_is_rejected_before_copy() {
        let document = Rc::new(RefCell::new(HtmlDocument::parse("<p id='x'></p>")));
        let oversized = "x".repeat(MAX_STRING_BYTES + 1);
        {
            let mut dom = document.borrow_mut();
            let id = dom.element_by_id("x").expect("paragraph");
            dom.set_attribute(id, "data", &oversized);
        }
        let events = Rc::new(RefCell::new(EventRegistry::default()));
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(DomHost::new(&document, events)));
        runtime.define_global("document", Value::Host("document".into()));
        let value = runtime
            .eval("document.getElementById('x').getAttribute('data')")
            .expect("getAttribute must refuse the oversized value");
        assert_eq!(value.display(), "null");
        assert!(runtime.memory_used() < 5000, "the oversized attribute must not be materialised");
    }

    #[test]
    fn title_and_text_content_reads_consume_dom_work() {
        // Measure the exact work a title read costs with a generous allowance.
        let document = Rc::new(RefCell::new(HtmlDocument::parse("<title>hello</title>")));
        let events = Rc::new(RefCell::new(EventRegistry::default()));
        let host = DomHost::new(&document, events);
        host.set_work_budget(1000);
        let work = host.work_cell();
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(host));
        runtime.define_global("document", Value::Host("document".into()));
        assert_eq!(runtime.eval("document.title").expect("title").display(), "hello");
        let title_cost = 1000 - work.get();
        assert!(title_cost > 0, "the title traversal must consume DOM work");
        assert_eq!(work.get(), 1000 - title_cost);

        // Exactly enough work still succeeds; one unit less is an explicit
        // error, and the runtime remains usable afterwards.
        let host = DomHost::new(&document, Rc::new(RefCell::new(EventRegistry::default())));
        host.set_work_budget(title_cost);
        let work = host.work_cell();
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(host));
        runtime.define_global("document", Value::Host("document".into()));
        assert_eq!(runtime.eval("document.title").expect("exact budget").display(), "hello");
        assert_eq!(work.get(), 0);

        let host = DomHost::new(&document, Rc::new(RefCell::new(EventRegistry::default())));
        host.set_work_budget(title_cost - 1);
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(host));
        runtime.define_global("document", Value::Host("document".into()));
        let error = runtime.eval("document.title").expect_err("insufficient budget");
        assert!(error.message.contains("DOM work budget"), "unexpected error: {}", error.message);
        assert_eq!(runtime.eval("1 + 1").expect("runtime still usable").display(), "2");

        // The same single-pass accounting applies to textContent.
        let document = Rc::new(RefCell::new(HtmlDocument::parse("<p id='x'>text</p>")));
        let host = DomHost::new(&document, Rc::new(RefCell::new(EventRegistry::default())));
        host.set_work_budget(1000);
        let work = host.work_cell();
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(host));
        runtime.define_global("document", Value::Host("document".into()));
        assert_eq!(
            runtime
                .eval("document.getElementById('x').textContent")
                .expect("textContent")
                .display(),
            "text"
        );
        assert!(work.get() < 1000, "the text walk must consume DOM work");

        let host = DomHost::new(&document, Rc::new(RefCell::new(EventRegistry::default())));
        host.set_work_budget(5);
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(host));
        runtime.define_global("document", Value::Host("document".into()));
        let error = runtime
            .eval("document.getElementById('x').textContent")
            .expect_err("textContent must report exhaustion instead of a partial value");
        assert!(error.message.contains("DOM work budget"), "unexpected error: {}", error.message);
        assert_eq!(runtime.eval("1 + 1").expect("runtime still usable").display(), "2");
    }

    #[test]
    fn attribute_scans_consume_dom_work_and_report_exhaustion() {
        let document = Rc::new(RefCell::new(HtmlDocument::parse("<p id='x'></p>")));
        {
            let mut dom = document.borrow_mut();
            let id = dom.element_by_id("x").expect("paragraph");
            for index in 0..50 {
                dom.set_attribute(id, &format!("data-{index}"), "value");
            }
        }
        let events = Rc::new(RefCell::new(EventRegistry::default()));
        let host = DomHost::new(&document, events);
        host.set_work_budget(10);
        let work = host.work_cell();
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(host));
        runtime.define_global("document", Value::Host("document".into()));
        let error = runtime
            .eval("document.querySelectorAll('#missing')")
            .expect_err("an exhausted attribute traversal must be reported");
        assert!(error.message.contains("DOM work budget"), "unexpected error: {}", error.message);
        assert_eq!(work.get(), 0);
    }

    #[test]
    fn oversized_existing_style_is_rejected_before_rebuilding() {
        let document = Rc::new(RefCell::new(HtmlDocument::parse("<p id='x'></p>")));
        {
            let mut dom = document.borrow_mut();
            let id = dom.element_by_id("x").expect("paragraph");
            dom.set_attribute(
                id,
                "style",
                &format!("background: {}", "x".repeat(MAX_STRING_BYTES)),
            );
        }
        let events = Rc::new(RefCell::new(EventRegistry::default()));
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(DomHost::new(&document, events)));
        runtime.define_global("document", Value::Host("document".into()));
        let error = runtime
            .eval("document.getElementById('x').style.color = 'red';")
            .expect_err("style rebuilding must be refused before allocating");
        assert!(error.message.contains("string length"), "unexpected error: {}", error.message);
    }

    #[test]
    fn title_and_text_content_reserve_memory_before_materialising() {
        let document = Rc::new(RefCell::new(HtmlDocument::parse("<title>hello</title>")));
        let events = Rc::new(RefCell::new(EventRegistry::default()));
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(DomHost::new(&document, events)));
        runtime.define_global("document", Value::Host("document".into()));
        runtime.set_memory_budget(1_000_000);
        assert_eq!(runtime.eval("document.title").expect("title").display(), "hello");
        let exact = runtime.memory_used();

        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(DomHost::new(
            &document,
            Rc::new(RefCell::new(EventRegistry::default())),
        )));
        runtime.define_global("document", Value::Host("document".into()));
        runtime.set_memory_budget(exact);
        assert_eq!(runtime.eval("document.title").expect("exact title budget").display(), "hello");

        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(DomHost::new(
            &document,
            Rc::new(RefCell::new(EventRegistry::default())),
        )));
        runtime.define_global("document", Value::Host("document".into()));
        runtime.set_memory_budget(exact - 1);
        let error = runtime.eval("document.title").expect_err("title must not be materialised");
        assert!(error.message.contains("memory budget"), "unexpected error: {}", error.message);

        let document = Rc::new(RefCell::new(HtmlDocument::parse("<p id='x'>text</p>")));
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(DomHost::new(
            &document,
            Rc::new(RefCell::new(EventRegistry::default())),
        )));
        runtime.define_global("document", Value::Host("document".into()));
        runtime.set_memory_budget(1_000_000);
        assert_eq!(
            runtime
                .eval("document.getElementById('x').textContent")
                .expect("textContent")
                .display(),
            "text"
        );
        let exact = runtime.memory_used();

        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(DomHost::new(
            &document,
            Rc::new(RefCell::new(EventRegistry::default())),
        )));
        runtime.define_global("document", Value::Host("document".into()));
        runtime.set_memory_budget(exact);
        assert_eq!(
            runtime
                .eval("document.getElementById('x').textContent")
                .expect("exact textContent budget")
                .display(),
            "text"
        );

        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(DomHost::new(
            &document,
            Rc::new(RefCell::new(EventRegistry::default())),
        )));
        runtime.define_global("document", Value::Host("document".into()));
        runtime.set_memory_budget(exact - 1);
        let error = runtime
            .eval("document.getElementById('x').textContent")
            .expect_err("textContent must not be materialised");
        assert!(error.message.contains("memory budget"), "unexpected error: {}", error.message);
    }

    #[test]
    fn single_element_selectors_report_budget_exhaustion() {
        let document = Rc::new(RefCell::new(HtmlDocument::parse("<p id='x'>text</p>")));
        let events = Rc::new(RefCell::new(EventRegistry::default()));
        let host = DomHost::new(&document, events);
        host.set_work_budget(1);
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(host));
        runtime.define_global("document", Value::Host("document".into()));

        for source in ["document.querySelector('p')", "document.getElementById('x')"] {
            let error = runtime.eval(source).expect_err("exhaustion must not look like null");
            assert!(
                error.message.contains("DOM work budget"),
                "unexpected error: {}",
                error.message
            );
        }
        assert_eq!(runtime.eval("1 + 1").expect("runtime still usable").display(), "2");
    }

    #[test]
    fn title_trim_reuses_the_reserved_buffer() {
        let document = Rc::new(RefCell::new(HtmlDocument::parse("<title>  hello  </title>")));
        let events = Rc::new(RefCell::new(EventRegistry::default()));
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(DomHost::new(&document, events)));
        runtime.define_global("document", Value::Host("document".into()));
        runtime.set_memory_budget(1_000_000);
        assert_eq!(runtime.eval("document.title").expect("title").display(), "hello");
        let exact = runtime.memory_used();

        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(DomHost::new(
            &document,
            Rc::new(RefCell::new(EventRegistry::default())),
        )));
        runtime.define_global("document", Value::Host("document".into()));
        runtime.set_memory_budget(exact);
        assert_eq!(
            runtime.eval("document.title").expect("exact budget covers the trim").display(),
            "hello"
        );

        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(DomHost::new(
            &document,
            Rc::new(RefCell::new(EventRegistry::default())),
        )));
        runtime.define_global("document", Value::Host("document".into()));
        runtime.set_memory_budget(exact - 1);
        let error = runtime.eval("document.title").expect_err("no unreserved trim copy allowed");
        assert!(error.message.contains("memory budget"), "unexpected error: {}", error.message);
    }

    #[test]
    fn document_body_reads_report_budget_exhaustion() {
        let document = Rc::new(RefCell::new(HtmlDocument::parse("<html><body>hi</body></html>")));
        let events = Rc::new(RefCell::new(EventRegistry::default()));
        let host = DomHost::new(&document, events);
        host.set_work_budget(1);
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(host));
        runtime.define_global("document", Value::Host("document".into()));

        for property in ["body", "documentElement"] {
            let source = format!("document.{property}");
            let error = runtime.eval(&source).expect_err("exhaustion must be explicit");
            assert!(
                error.message.contains("DOM work budget"),
                "unexpected error: {}",
                error.message
            );
        }
        assert_eq!(runtime.eval("1 + 1").expect("runtime still usable").display(), "2");
    }

    #[test]
    fn host_handles_reserve_memory_before_materialisation() {
        let document = Rc::new(RefCell::new(HtmlDocument::parse(
            "<html><body><main id='x'><p></p><p></p></main></body></html>",
        )));
        for source in [
            "document.body",
            "document.documentElement",
            "document.querySelector('main')",
            "document.getElementById('x')",
            "document.querySelectorAll('p').length",
        ] {
            let (_, exact) = eval_with_budget(&document, source, 1_000_000)
                .unwrap_or_else(|error| panic!("{source}: {error}"));
            let (value, used) = eval_with_budget(&document, source, exact)
                .unwrap_or_else(|error| panic!("{source} with exact budget: {error}"));
            assert!(!value.is_empty(), "{source} returned no value");
            assert_eq!(used, exact, "{source} must reserve exactly the measured cost");

            let error = eval_with_budget(&document, source, exact - 1).unwrap_err();
            assert!(
                error.message.contains("memory budget"),
                "{source}: unexpected error: {}",
                error.message
            );
        }
    }

    #[test]
    fn document_element_returns_the_active_html_root() {
        let document = Rc::new(RefCell::new(HtmlDocument::parse(
            "<html><body><p id='x'>hi</p></body></html>",
        )));
        let events = Rc::new(RefCell::new(EventRegistry::default()));
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(DomHost::new(&document, events)));
        runtime.define_global("document", Value::Host("document".into()));
        assert_eq!(
            runtime
                .eval("document.documentElement === document.querySelector('html')")
                .expect("documentElement")
                .display(),
            "true"
        );
        assert_eq!(
            runtime.eval("document.body !== document.documentElement").expect("distinct").display(),
            "true"
        );
    }

    #[test]
    fn element_remove_detaches_and_can_be_reappended() {
        let document = Rc::new(RefCell::new(HtmlDocument::parse("<main></main>")));
        let events = Rc::new(RefCell::new(EventRegistry::default()));
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(DomHost::new(&document, events)));
        runtime.define_global("document", Value::Host("document".into()));
        runtime
            .eval("let p = document.createElement('p'); p.textContent = 'hi'; document.body.appendChild(p);")
            .expect("create and attach");
        assert_eq!(
            runtime.eval("document.querySelectorAll('p').length").expect("count").display(),
            "1"
        );
        runtime.eval("p.remove();").expect("remove");
        runtime.eval("p.remove();").expect("removing a detached node is a no-op");
        assert_eq!(
            runtime.eval("document.querySelectorAll('p').length").expect("count").display(),
            "0"
        );
        runtime.eval("document.body.appendChild(p);").expect("re-append");
        assert_eq!(
            runtime.eval("document.querySelectorAll('p').length").expect("count").display(),
            "1"
        );
        assert_eq!(runtime.eval("p.textContent").expect("subtree survived").display(), "hi");

        let error = runtime.eval("document.documentElement.remove()").unwrap_err();
        assert!(error.message.contains("top-level element"), "{}", error.message);
        assert_eq!(runtime.eval("1 + 1").expect("runtime usable").display(), "2");
    }

    #[test]
    fn create_element_exhaustion_is_explicit() {
        let document = Rc::new(RefCell::new(HtmlDocument::parse("<main></main>")));
        let events = Rc::new(RefCell::new(EventRegistry::default()));
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(DomHost::new(&document, events)));
        runtime.define_global("document", Value::Host("document".into()));
        let error = runtime
            .eval("for (let i = 0; i < 10000; i = i + 1) { document.createElement('i'); } document.createElement('b');")
            .expect_err("the dynamic node budget must be reported");
        assert!(
            error.message.contains("element budget exhausted"),
            "unexpected error: {}",
            error.message
        );
    }

    #[test]
    fn remove_consumes_dom_work_and_rejects_invalid_handles() {
        let document = Rc::new(RefCell::new(HtmlDocument::parse("<main id='m'></main>")));
        let events = Rc::new(RefCell::new(EventRegistry::default()));
        let host = DomHost::new(&document, events);
        let work = host.work_cell();
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(host));
        runtime.define_global("document", Value::Host("document".into()));
        runtime
            .eval("let main = document.getElementById('m'); let first = null; for (let i = 0; i < 1000; i = i + 1) { let child = document.createElement('i'); main.appendChild(child); if (i === 0) { first = child; } }")
            .expect("build a wide parent");

        work.set(5);
        let error = runtime
            .eval("first.remove()")
            .expect_err("a wide sibling scan must consume the DOM work budget");
        assert!(error.message.contains("DOM work budget"), "unexpected error: {}", error.message);
        assert_eq!(work.get(), 0);
        work.set(1_000_000);
        assert_eq!(
            runtime.eval("document.querySelectorAll('i').length").expect("unchanged").display(),
            "1000"
        );

        runtime.define_global("stale", Value::Host("element:9999999".into()));
        let error = runtime.eval("stale.remove()").unwrap_err();
        assert!(
            error.message.contains("invalid element handle"),
            "unexpected error: {}",
            error.message
        );
    }

    #[test]
    fn append_prepend_remove_child_and_replace_child_work_in_javascript() {
        let document = Rc::new(RefCell::new(HtmlDocument::parse("<html><body></body></html>")));
        let events = Rc::new(RefCell::new(EventRegistry::default()));
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(DomHost::new(&document, events)));
        runtime.define_global("document", Value::Host("document".into()));
        runtime
            .eval("let box = document.createElement('div'); document.body.appendChild(box); let a = document.createElement('i'); a.textContent = 'a'; let b = document.createElement('i'); b.textContent = 'b'; let c = document.createElement('i'); c.textContent = 'c';")
            .expect("setup");

        runtime.eval("box.append(a, b); box.prepend(c);").expect("append and prepend");
        assert_eq!(runtime.eval("box.textContent").expect("order").display(), "cab");

        let removed = runtime.eval("box.removeChild(a)").expect("removeChild");
        assert_eq!(removed.display(), "[host element:5]", "the removed handle format");
        assert_eq!(runtime.eval("a.textContent").expect("removed handle").display(), "a");
        assert_eq!(runtime.eval("box.textContent").expect("after remove").display(), "cb");

        runtime
            .eval("let x = document.createElement('i'); x.textContent = 'x';")
            .expect("new child");
        let old = runtime.eval("box.replaceChild(x, b)").expect("replaceChild");
        assert_eq!(old.display(), "[host element:7]", "the replaced handle format");
        assert_eq!(runtime.eval("b.textContent").expect("old handle").display(), "b");
        assert_eq!(runtime.eval("box.textContent").expect("after replace").display(), "cx");
    }

    #[test]
    fn dom_mutation_errors_are_explicit_and_do_not_change_the_dom() {
        let document = Rc::new(RefCell::new(HtmlDocument::parse("<html><body></body></html>")));
        let events = Rc::new(RefCell::new(EventRegistry::default()));
        let host = DomHost::new(&document, events);
        let work = host.work_cell();
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(host));
        runtime.define_global("document", Value::Host("document".into()));
        runtime.define_global("stale", Value::Host("element:9999999".into()));
        runtime
            .eval("let box = document.createElement('div'); document.body.appendChild(box); let a = document.createElement('i'); a.textContent = 'a'; box.append(a); let grand = document.createElement('i'); a.appendChild(grand);")
            .expect("setup");
        let before = runtime.eval("box.textContent").expect("before").display();

        let error = runtime.eval("box.removeChild(grand)").unwrap_err();
        assert!(error.message.contains("not a child"), "{}", error.message);
        let statements = parse("box.replaceChild(a, grand)").expect("parse");
        let error = runtime.run(&statements).unwrap_err();
        assert!(error.message.contains("already attached"), "{}", error.message);
        let error = runtime.eval("box.removeChild(stale)").unwrap_err();
        assert!(error.message.contains("invalid element handle"), "{}", error.message);
        let error = runtime.eval("box.append(a)").unwrap_err();
        assert!(error.message.contains("already attached"), "{}", error.message);
        assert_eq!(runtime.eval("box.textContent").expect("intact").display(), before);

        work.set(1);
        let error = runtime.eval("let d = document.createElement('i'); box.append(d)").unwrap_err();
        assert!(error.message.contains("DOM work budget"), "{}", error.message);
        work.set(1_000_000);
        assert_eq!(runtime.eval("box.textContent").expect("intact").display(), before);

        let before_memory = runtime.memory_used();
        let statements = parse("box.removeChild(a)").expect("parse");
        let removed = runtime.run(&statements).expect("successful removeChild");
        assert!(removed.display().starts_with("[host"), "the removed handle is returned");
        assert!(runtime.memory_used() > before_memory, "a successful handle must be reserved");
        assert_eq!(runtime.eval("1 + 1").expect("runtime usable").display(), "2");
    }

    #[test]
    fn failed_mutations_do_not_reserve_the_returned_handle() {
        let document_ok = Rc::new(RefCell::new(HtmlDocument::parse("<html><body></body></html>")));
        let mut runtime_ok = Runtime::new();
        runtime_ok.set_host(Box::new(DomHost::new(
            &document_ok,
            Rc::new(RefCell::new(EventRegistry::default())),
        )));
        runtime_ok.define_global("document", Value::Host("document".into()));
        runtime_ok
            .eval("let box = document.createElement('div'); document.body.appendChild(box); let n = document.createElement('i'); n.textContent = 'n'; box.appendChild(n); let other = document.createElement('i'); other.textContent = 'o';")
            .expect("setup success");
        let before_ok = runtime_ok.memory_used();
        let statements = parse("box.removeChild(n)").expect("parse");
        let removed = runtime_ok.run(&statements).expect("successful removeChild");
        assert!(removed.display().starts_with("[host"));
        let delta_ok = runtime_ok.memory_used() - before_ok;

        let document_bad = Rc::new(RefCell::new(HtmlDocument::parse("<html><body></body></html>")));
        let mut runtime_bad = Runtime::new();
        runtime_bad.set_host(Box::new(DomHost::new(
            &document_bad,
            Rc::new(RefCell::new(EventRegistry::default())),
        )));
        runtime_bad.define_global("document", Value::Host("document".into()));
        runtime_bad
            .eval("let box = document.createElement('div'); document.body.appendChild(box); let n = document.createElement('i'); n.textContent = 'n'; let other = document.createElement('i'); other.textContent = 'o'; box.appendChild(other); other.appendChild(n);")
            .expect("setup failure");
        let before_bad = runtime_bad.memory_used();
        let statements = parse("box.removeChild(n)").expect("parse");
        let error = runtime_bad.run(&statements).unwrap_err();
        assert!(error.message.contains("not a child"), "{}", error.message);
        let delta_bad = runtime_bad.memory_used() - before_bad;
        assert!(
            delta_ok > delta_bad,
            "a failed removeChild must not reserve the handle ({delta_bad} vs {delta_ok})"
        );

        // Same check for replaceChild: `other` is detached in the first runtime
        // and already attached in the second, with an identical script.
        let document_ok2 = Rc::new(RefCell::new(HtmlDocument::parse("<html><body></body></html>")));
        let mut runtime_ok2 = Runtime::new();
        runtime_ok2.set_host(Box::new(DomHost::new(
            &document_ok2,
            Rc::new(RefCell::new(EventRegistry::default())),
        )));
        runtime_ok2.define_global("document", Value::Host("document".into()));
        runtime_ok2
            .eval("let box = document.createElement('div'); document.body.appendChild(box); let n = document.createElement('i'); n.textContent = 'n'; box.appendChild(n); let other = document.createElement('i'); other.textContent = 'o';")
            .expect("setup success 2");
        let before_ok2 = runtime_ok2.memory_used();
        let statements = parse("box.replaceChild(other, n)").expect("parse");
        let old = runtime_ok2.run(&statements).expect("successful replaceChild");
        assert!(old.display().starts_with("[host"));
        let delta_ok2 = runtime_ok2.memory_used() - before_ok2;

        let document_bad2 =
            Rc::new(RefCell::new(HtmlDocument::parse("<html><body></body></html>")));
        let mut runtime_bad2 = Runtime::new();
        runtime_bad2.set_host(Box::new(DomHost::new(
            &document_bad2,
            Rc::new(RefCell::new(EventRegistry::default())),
        )));
        runtime_bad2.define_global("document", Value::Host("document".into()));
        runtime_bad2
            .eval("let box = document.createElement('div'); document.body.appendChild(box); let n = document.createElement('i'); n.textContent = 'n'; box.appendChild(n); let other = document.createElement('i'); other.textContent = 'o'; box.appendChild(other);")
            .expect("setup failure 2");
        let before_bad2 = runtime_bad2.memory_used();
        let statements = parse("box.replaceChild(other, n)").expect("parse");
        let error = runtime_bad2.run(&statements).unwrap_err();
        assert!(error.message.contains("already attached"), "{}", error.message);
        let delta_bad2 = runtime_bad2.memory_used() - before_bad2;
        assert!(
            delta_ok2 > delta_bad2,
            "a failed replaceChild must not reserve the handle ({delta_bad2} vs {delta_ok2})"
        );
    }

    #[test]
    fn class_list_supports_multi_arguments_and_strict_tokens() {
        let document = Rc::new(RefCell::new(HtmlDocument::parse("<html><body></body></html>")));
        let events = Rc::new(RefCell::new(EventRegistry::default()));
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(DomHost::new(&document, events)));
        runtime.define_global("document", Value::Host("document".into()));
        runtime
            .eval("let el = document.createElement('div'); document.body.appendChild(el);")
            .expect("setup");

        runtime.eval("el.classList.add('a', 'a', 'b');").expect("duplicates");
        assert_eq!(runtime.eval("el.getAttribute('class')").expect("class").display(), "a b");
        runtime.eval("el.classList.remove('a', 'missing');").expect("remove");
        assert_eq!(runtime.eval("el.getAttribute('class')").expect("class").display(), "b");
        assert_eq!(
            runtime.eval("el.classList.toggle('c', true)").expect("toggle on").display(),
            "true"
        );
        assert_eq!(runtime.eval("el.getAttribute('class')").expect("class").display(), "b c");
        assert_eq!(
            runtime.eval("el.classList.toggle('c', false)").expect("toggle off").display(),
            "false"
        );
        assert_eq!(runtime.eval("el.getAttribute('class')").expect("class").display(), "b");
        assert_eq!(
            runtime.eval("el.classList.replace('b', 'd')").expect("replace").display(),
            "true"
        );
        assert_eq!(runtime.eval("el.classList.value").expect("value").display(), "d");
        assert_eq!(runtime.eval("el.classList.length").expect("length").display(), "1");
        assert_eq!(runtime.eval("el.classList.item(0)").expect("item").display(), "d");

        let error = runtime.eval("el.classList.add('ok', 'bad token')").unwrap_err();
        assert!(error.message.contains("invalid class token"), "{}", error.message);
        assert_eq!(runtime.eval("el.getAttribute('class')").expect("intact").display(), "d");
        let error = runtime.eval("el.classList.add('')").unwrap_err();
        assert!(error.message.contains("invalid class token"), "{}", error.message);
        assert_eq!(runtime.eval("el.getAttribute('class')").expect("intact").display(), "d");
    }

    #[test]
    fn dataset_converts_both_directions_and_rejects_bad_keys() {
        let document = Rc::new(RefCell::new(HtmlDocument::parse("<html><body></body></html>")));
        let events = Rc::new(RefCell::new(EventRegistry::default()));
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(DomHost::new(&document, events)));
        runtime.define_global("document", Value::Host("document".into()));
        runtime
            .eval("let el = document.createElement('div'); document.body.appendChild(el);")
            .expect("setup");

        runtime.eval("el.dataset.fooBar = 'v';").expect("dataset write");
        assert_eq!(
            runtime.eval("el.getAttribute('data-foo-bar')").expect("attribute").display(),
            "v"
        );
        assert_eq!(runtime.eval("el.dataset.fooBar").expect("dataset read").display(), "v");
        assert_eq!(runtime.eval("el.dataset.length").expect("dataset length").display(), "1");
        assert_eq!(runtime.eval("el.dataset[0]").expect("dataset key").display(), "fooBar");

        runtime.eval("el.dataset.URL = 'x';").expect("uppercase key");
        assert_eq!(
            runtime.eval("el.getAttribute('data-u-r-l')").expect("attribute").display(),
            "x"
        );

        let error = runtime.eval("el.dataset['bad-key'] = 'x'").unwrap_err();
        assert!(error.message.contains("invalid dataset key"), "{}", error.message);
        let error = runtime.eval("el.dataset['']").unwrap_err();
        assert!(error.message.contains("invalid dataset key"), "{}", error.message);
        assert_eq!(
            runtime.eval("el.getAttribute('data-bad-key')").expect("no write").display(),
            "null"
        );
    }

    #[test]
    fn class_and_dataset_mutations_honor_budgets() {
        let document = Rc::new(RefCell::new(HtmlDocument::parse("<html><body></body></html>")));
        let events = Rc::new(RefCell::new(EventRegistry::default()));
        let host = DomHost::new(&document, events);
        let work = host.work_cell();
        let mut runtime = Runtime::new();
        runtime.set_host(Box::new(host));
        runtime.define_global("document", Value::Host("document".into()));
        runtime.eval("let el = document.createElement('div'); document.body.appendChild(el); el.classList.add('a');").expect("setup");

        work.set(0);
        let statements = parse("el.classList.add('x')").expect("parse");
        let error = runtime.run(&statements).unwrap_err();
        assert!(error.message.contains("DOM work budget"), "{}", error.message);
        work.set(1_000_000);
        assert_eq!(runtime.eval("el.getAttribute('class')").expect("intact").display(), "a");

        runtime.set_memory_budget(runtime.memory_used());
        let statements = parse("el.classList.add('x')").expect("parse");
        let error = runtime.run(&statements).unwrap_err();
        assert!(error.message.contains("memory budget"), "{}", error.message);
        runtime.set_memory_budget(1_000_000);
        assert_eq!(runtime.eval("el.getAttribute('class')").expect("intact").display(), "a");
        assert_eq!(runtime.eval("1 + 1").expect("runtime usable").display(), "2");
    }
}
