//! DOM host bridge and event dispatch for the Rust JavaScript runtime.

use std::{
    cell::RefCell,
    collections::HashMap,
    rc::{Rc, Weak},
};

use bweb_html::HtmlDocument;
use bweb_js::{Host, JsError, Object, Value};

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

/// Exposes a document to scripts through the `document` host object.
pub struct DomHost {
    document: Weak<RefCell<HtmlDocument>>,
    events: Rc<RefCell<EventRegistry>>,
}

impl DomHost {
    #[must_use]
    pub fn new(document: &Rc<RefCell<HtmlDocument>>, events: Rc<RefCell<EventRegistry>>) -> Self {
        Self { document: Rc::downgrade(document), events }
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

fn style_value(style: &str, property: &str) -> Option<String> {
    style.split(';').find_map(|declaration| {
        let (name, value) = declaration.split_once(':')?;
        (name.trim() == property).then(|| value.trim().to_owned())
    })
}

fn with_style(document: &mut HtmlDocument, index: usize, property: &str, value: Option<&str>) {
    let Some(id) = document.node_at(index) else { return };
    let style = document.attribute(id, "style").unwrap_or("").to_owned();
    let mut declarations: Vec<(String, String)> = style
        .split(';')
        .filter_map(|part| {
            let (name, value) = part.split_once(':')?;
            Some((name.trim().to_owned(), value.trim().to_owned()))
        })
        .collect();
    declarations.retain(|(name, _)| name != property);
    if let Some(value) = value {
        declarations.push((property.to_owned(), value.to_owned()));
    }
    let style = declarations
        .iter()
        .map(|(name, value)| format!("{name}: {value}"))
        .collect::<Vec<_>>()
        .join("; ");
    document.set_attribute(id, "style", &style);
}

impl Host for DomHost {
    fn get(&self, object: &str, property: &str) -> Value {
        let Some(document) = self.document.upgrade() else { return Value::Undefined };
        let document = document.borrow();
        if object == "document" {
            return match property {
                "title" => Value::String(document.title().unwrap_or_default()),
                "body" | "documentElement" => document
                    .iter()
                    .find_map(|(id, node)| {
                        let tag = match node.kind() {
                            bweb_html::NodeKind::Element { tag_name, .. } => tag_name.as_str(),
                            _ => return None,
                        };
                        (tag == property).then_some(Value::Host(format!("element:{}", id.index())))
                    })
                    .unwrap_or(Value::Undefined),
                _ => Value::Undefined,
            };
        }
        if let Some(index) = handle_index(object, "element:") {
            let Some(id) = document.node_at(index) else { return Value::Undefined };
            return match property {
                "style" => Value::Host(format!("style:{index}")),
                "textContent" => Value::String(document.text_content(id)),
                "classList" => Value::Host(format!("classlist:{index}")),
                "id" | "class" | "href" | "src" => {
                    Value::String(document.attribute(id, property).unwrap_or("").to_owned())
                }
                _ => Value::Undefined,
            };
        }
        if let Some(index) = handle_index(object, "style:") {
            if let Some(id) = document.node_at(index) {
                let style = document.attribute(id, "style").unwrap_or("");
                return style_value(style, property).map_or(Value::Undefined, Value::String);
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

    fn set(&mut self, object: &str, property: &str, value: Value) -> Result<(), JsError> {
        let Some(document) = self.document.upgrade() else { return Ok(()) };
        let mut document = document.borrow_mut();
        if object == "document" && property == "title" {
            document.set_title(value.display());
            return Ok(());
        }
        if let Some(index) = handle_index(object, "element:") {
            let Some(id) = document.node_at(index) else { return Ok(()) };
            match property {
                "textContent" => document.set_text_content(id, &value.display()),
                "id" | "class" | "href" | "src" | "style" => {
                    document.set_attribute(id, property, &value.display());
                }
                _ => {}
            }
            return Ok(());
        }
        if let Some(index) = handle_index(object, "style:") {
            with_style(&mut document, index, property, Some(&value.display()));
        }
        Ok(())
    }

    fn call(&mut self, object: &str, method: &str, args: &[Value]) -> Result<Value, JsError> {
        if let Some(index) = handle_index(object, "classlist:") {
            let class = args.first().map_or(String::new(), Value::display);
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
            let event = args.first().map_or(String::new(), Value::display);
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
            let Some(id) = document.node_at(index) else { return Ok(Value::Undefined) };
            return match method {
                "setAttribute" => {
                    if let Some(name) = args.first() {
                        let value = args.get(1).map_or(String::new(), Value::display);
                        document.set_attribute(id, &name.display(), &value);
                    }
                    Ok(Value::Undefined)
                }
                "getAttribute" => {
                    let name = args.first().map_or(String::new(), Value::display);
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
                _ => Ok(Value::Undefined),
            };
        }
        if object == "document" {
            match method {
                "querySelectorAll" => {
                    let Some(selector) = args.first().map(Value::display) else {
                        return Ok(Value::Object(Rc::new(RefCell::new(Object {
                            properties: HashMap::from([("length".into(), Value::Number(0.0))]),
                            is_array: true,
                        }))));
                    };
                    let Some(document) = self.document.upgrade() else {
                        return Ok(Value::Undefined);
                    };
                    let document = document.borrow();
                    let nodes = document.query_selector_all(&selector);
                    let mut properties = HashMap::new();
                    for (index, node) in nodes.iter().enumerate() {
                        properties.insert(
                            index.to_string(),
                            Value::Host(format!("element:{}", node.index())),
                        );
                    }
                    properties.insert("length".into(), Value::Number(nodes.len() as f64));
                    return Ok(Value::Object(Rc::new(RefCell::new(Object {
                        properties,
                        is_array: true,
                    }))));
                }
                "createElement" => {
                    let Some(tag) = args.first().map(Value::display) else {
                        return Ok(Value::Null);
                    };
                    let Some(document) = self.document.upgrade() else {
                        return Ok(Value::Undefined);
                    };
                    let mut document = document.borrow_mut();
                    let node = document.create_element(&tag);
                    return Ok(Value::Host(format!("element:{}", node.index())));
                }
                "getElementById" | "querySelector" => {
                    let Some(selector) = args.first().map(Value::display) else {
                        return Ok(Value::Null);
                    };
                    let Some(document) = self.document.upgrade() else {
                        return Ok(Value::Undefined);
                    };
                    let document = document.borrow();
                    let found = if method == "getElementById" {
                        document.element_by_id(&selector)
                    } else {
                        document.query_selector(&selector)
                    };
                    return Ok(found.map_or(Value::Null, |node| {
                        Value::Host(format!("element:{}", node.index()))
                    }));
                }
                "write" => {
                    let text = args.iter().map(Value::display).collect::<Vec<_>>().join(" ");
                    let Some(document) = self.document.upgrade() else {
                        return Ok(Value::Undefined);
                    };
                    let mut document = document.borrow_mut();
                    let body = document.iter().find_map(|(id, node)| {
                        matches!(node.kind(), bweb_html::NodeKind::Element { tag_name, .. } if tag_name == "body")
                            .then_some(id)
                    });
                    if let Some(body) = body {
                        document.set_text_content(body, &text);
                    }
                }
                _ => {}
            }
        }
        Ok(Value::Undefined)
    }
}

#[cfg(test)]
mod tests {
    use super::{DomHost, EventRegistry};
    use bweb_html::{HtmlDocument, NodeKind};
    use bweb_js::{Runtime, Value};
    use std::{cell::RefCell, rc::Rc};

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
}
