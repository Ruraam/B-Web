//! HTML document, arena-backed DOM, and policy-gated document resource loading.
//!
//! The parser implements a documented subset of HTML5 tree construction:
//! insertion modes, implied elements, entity decoding, raw-text/RCDATA scanning,
//! formatting-element reconstruction, foster parenting and inert template
//! content. The complete tokenizer and adoption-agency inner loop remain in the
//! roadmap. All network access is delegated to bweb-net's HttpTransport.

mod entities;

use bweb_net::{HttpTransport, HttpTransportError};
use bweb_shield::ResourceType;
use reqwest::header::{CONTENT_TYPE, HeaderMap};
use std::{
    collections::HashSet,
    error::Error,
    fmt,
    io::{self, Read},
};
use url::Url;

const MAX_DOCUMENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_STYLESHEET_BYTES: usize = 4 * 1024 * 1024;
const MAX_SCRIPT_BYTES: usize = 8 * 1024 * 1024;
const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
/// Maximum depth of the DOM tree, including nodes appended by scripts.
pub const MAX_DOM_DEPTH: usize = 128;
/// Maximum number of nodes scripts may add to one document.
const MAX_DYNAMIC_NODES: usize = 10_000;
/// Cumulative download budget for one page, including the main document.
const MAX_TOTAL_RESOURCE_BYTES: usize = 32 * 1024 * 1024;
/// Maximum number of distinct subresources fetched for one page.
const MAX_TOTAL_RESOURCES: usize = 256;

/// Stable index into a document's node arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(usize);

impl NodeId {
    /// Returns the arena index. IDs remain stable while nodes are only appended.
    #[must_use]
    pub const fn index(self) -> usize {
        self.0
    }
}

/// An HTML attribute as stored in the DOM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribute {
    pub name: String,
    pub value: String,
}

/// A node stored in the document arena.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    kind: NodeKind,
    parent: Option<NodeId>,
    children: Vec<NodeId>,
}

impl Node {
    #[must_use]
    pub fn kind(&self) -> &NodeKind {
        &self.kind
    }

    #[must_use]
    pub const fn parent(&self) -> Option<NodeId> {
        self.parent
    }

    #[must_use]
    pub fn children(&self) -> &[NodeId] {
        &self.children
    }
}

/// The supported node kinds in the initial DOM foundation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    Document,
    Element {
        tag_name: String,
        attributes: Vec<Attribute>,
    },
    Text(String),
    Comment(String),
    /// The inert content root of a `<template>` element.
    TemplateContent,
}

/// Arena-backed HTML document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HtmlDocument {
    source: String,
    nodes: Vec<Node>,
    root: NodeId,
    title_override: Option<String>,
    /// Number of nodes created through the dynamic DOM API.
    dynamic_nodes: usize,
}

impl Default for HtmlDocument {
    fn default() -> Self {
        Self::parse("")
    }
}

impl HtmlDocument {
    /// Parses markup into an arena-backed DOM.
    #[must_use]
    pub fn parse(source: impl Into<String>) -> Self {
        let source = source.into();
        let root = NodeId(0);
        let mut document = Self {
            source,
            nodes: vec![Node { kind: NodeKind::Document, parent: None, children: Vec::new() }],
            root,
            title_override: None,
            dynamic_nodes: 0,
        };
        document.build_tree();
        document
    }

    /// Returns the original markup.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Returns the document root ID.
    #[must_use]
    pub const fn root(&self) -> NodeId {
        self.root
    }

    /// Looks up a node by its stable arena ID.
    #[must_use]
    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id.0)
    }

    /// Validates an arena index and returns its stable ID.
    #[must_use]
    pub fn node_at(&self, index: usize) -> Option<NodeId> {
        (index < self.nodes.len()).then_some(NodeId(index))
    }

    /// Returns an iterator over arena nodes in stable document order.
    pub fn iter(&self) -> impl Iterator<Item = (NodeId, &Node)> {
        self.nodes.iter().enumerate().map(|(index, node)| (NodeId(index), node))
    }

    /// Returns the number of nodes, including the document root.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Returns the first title element's text content, if present.
    #[must_use]
    pub fn title(&self) -> Option<String> {
        if let Some(title) = &self.title_override {
            let title = title.trim().to_owned();
            return (!title.is_empty()).then_some(title);
        }
        let title_id =
            self.nodes.iter().enumerate().find_map(|(index, node)| match &node.kind {
                NodeKind::Element { tag_name, .. } if tag_name == "title" => Some(NodeId(index)),
                _ => None,
            })?;
        let mut text = String::new();
        self.collect_text(title_id, &mut text);
        let title = text.trim().to_owned();
        (!title.is_empty()).then_some(title)
    }

    /// Returns the first `<base href>` value, used to resolve relative URLs.
    #[must_use]
    pub fn base_href(&self) -> Option<&str> {
        self.nodes.iter().find_map(|node| match &node.kind {
            NodeKind::Element { tag_name, attributes } if tag_name == "base" => attributes
                .iter()
                .find(|attribute| attribute.name == "href")
                .map(|attribute| attribute.value.as_str()),
            _ => None,
        })
    }

    /// Returns the inert content root of a `<template>` element, if present.
    #[must_use]
    pub fn template_content(&self, template: NodeId) -> Option<NodeId> {
        self.node(template).and_then(|node| node.children().first()).copied().filter(|id| {
            matches!(self.node(*id).map(|node| node.kind()), Some(NodeKind::TemplateContent))
        })
    }

    /// Returns the text contents of inline <style> elements in document order.
    #[must_use]
    pub fn inline_stylesheets(&self) -> Vec<String> {
        self.nodes
            .iter()
            .enumerate()
            .filter(|(index, node)| {
                !self.is_in_template(NodeId(*index))
                    && matches!(
                        &node.kind,
                        NodeKind::Element { tag_name, .. } if tag_name == "style"
                    )
            })
            .map(|(index, _)| {
                let mut source = String::new();
                self.collect_text(NodeId(index), &mut source);
                source
            })
            .filter(|source| !source.trim().is_empty())
            .collect()
    }

    /// Returns the `href` of the nearest ancestor hyperlink of `id`, if any.
    ///
    /// The node itself is checked as well so callers may pass either a text
    /// node produced by layout or the `<a>` element directly. Empty `href`
    /// values are not treated as usable links.
    #[must_use]
    pub fn hyperlink_href(&self, id: NodeId) -> Option<&str> {
        let mut current = self.node(id)?;
        loop {
            if let NodeKind::Element { tag_name, attributes } = current.kind() {
                if tag_name == "a" {
                    if let Some(href) = attributes.iter().find(|attribute| attribute.name == "href")
                    {
                        let value = href.value.as_str();
                        if !value.trim().is_empty() {
                            return Some(value);
                        }
                    }
                }
            }
            current = self.node(current.parent()?)?;
        }
    }

    /// Replaces the document title reported by `title()`.
    pub fn set_title(&mut self, title: impl Into<String>) {
        self.title_override = Some(title.into());
    }

    /// Finds the first element carrying the given `id` attribute.
    #[must_use]
    pub fn element_by_id(&self, id: &str) -> Option<NodeId> {
        self.iter().find_map(|(node_id, node)| {
            if self.is_in_template(node_id) {
                return None;
            }
            match node.kind() {
                NodeKind::Element { attributes, .. } => attributes
                    .iter()
                    .find(|attribute| attribute.name == "id" && attribute.value == id)
                    .map(|_| node_id),
                _ => None,
            }
        })
    }

    /// Returns an element attribute value.
    #[must_use]
    pub fn attribute(&self, id: NodeId, name: &str) -> Option<&str> {
        match self.node(id).map(|node| node.kind()) {
            Some(NodeKind::Element { attributes, .. }) => attributes
                .iter()
                .find(|attribute| attribute.name == name)
                .map(|attribute| attribute.value.as_str()),
            _ => None,
        }
    }

    /// Sets or removes an element attribute. Returns whether the node is an element.
    pub fn set_attribute(&mut self, id: NodeId, name: &str, value: &str) -> bool {
        let Some(node) = self.nodes.get_mut(id.0) else { return false };
        let NodeKind::Element { attributes, .. } = &mut node.kind else { return false };
        if let Some(existing) = attributes.iter_mut().find(|attribute| attribute.name == name) {
            existing.value = value.to_owned();
        } else {
            attributes.push(Attribute { name: name.to_owned(), value: value.to_owned() });
        }
        true
    }

    /// Returns the element ancestors of a node, from parent to root.
    #[must_use]
    pub fn ancestor_elements(&self, id: NodeId) -> Vec<NodeId> {
        let mut result = Vec::new();
        let mut current = self.node(id).and_then(|node| node.parent());
        while let Some(parent) = current {
            if matches!(self.node(parent).map(|node| node.kind()), Some(NodeKind::Element { .. })) {
                result.push(parent);
            }
            current = self.node(parent).and_then(|node| node.parent());
        }
        result
    }

    /// Returns the concatenated text content of a node.
    #[must_use]
    pub fn text_content(&self, id: NodeId) -> String {
        let mut text = String::new();
        self.collect_text(id, &mut text);
        text
    }

    /// Replaces all children of a node with a single text node.
    pub fn set_text_content(&mut self, id: NodeId, text: &str) {
        if self.nodes.get(id.0).is_none() {
            return;
        }
        // Check both limits before touching the DOM: an exhausted budget must
        // not leave the element emptied with no replacement text.
        if self.dynamic_nodes >= MAX_DYNAMIC_NODES {
            return;
        }
        if self.node_depth(id).saturating_add(1) > MAX_DOM_DEPTH.saturating_add(1) {
            return;
        }
        self.nodes[id.0].children.clear();
        self.dynamic_nodes += 1;
        let child = NodeId(self.nodes.len());
        self.nodes.push(Node {
            kind: NodeKind::Text(text.to_owned()),
            parent: Some(id),
            children: Vec::new(),
        });
        self.nodes[id.0].children.push(child);
    }

    /// Finds the first element matching a simple tag, `#id` or `.class` selector.
    #[must_use]
    pub fn query_selector(&self, selector: &str) -> Option<NodeId> {
        let selector = selector.trim();
        if selector.is_empty() {
            return None;
        }
        self.iter().find_map(|(id, node)| {
            if self.is_in_template(id) {
                return None;
            }
            let NodeKind::Element { tag_name, attributes } = node.kind() else {
                return None;
            };
            let matches = if let Some(name) = selector.strip_prefix('#') {
                attributes.iter().any(|attribute| attribute.name == "id" && attribute.value == name)
            } else if let Some(name) = selector.strip_prefix('.') {
                attributes.iter().any(|attribute| {
                    attribute.name == "class"
                        && attribute.value.split_ascii_whitespace().any(|part| part == name)
                })
            } else {
                tag_name.eq_ignore_ascii_case(selector)
            };
            matches.then_some(id)
        })
    }

    /// Returns every element matching a simple tag, `#id` or `.class` selector
    /// in document order.
    #[must_use]
    pub fn query_selector_all(&self, selector: &str) -> Vec<NodeId> {
        let selector = selector.trim();
        if selector.is_empty() {
            return Vec::new();
        }
        self.iter()
            .filter(|(id, node)| {
                if self.is_in_template(*id) {
                    return false;
                }
                let NodeKind::Element { tag_name, attributes } = node.kind() else {
                    return false;
                };
                if let Some(name) = selector.strip_prefix('#') {
                    attributes
                        .iter()
                        .any(|attribute| attribute.name == "id" && attribute.value == name)
                } else if let Some(name) = selector.strip_prefix('.') {
                    attributes.iter().any(|attribute| {
                        attribute.name == "class"
                            && attribute.value.split_ascii_whitespace().any(|part| part == name)
                    })
                } else {
                    tag_name.eq_ignore_ascii_case(selector)
                }
            })
            .map(|(id, _)| id)
            .collect()
    }

    /// Creates a detached element node with no attributes.
    #[must_use]
    pub fn create_element(&mut self, tag_name: &str) -> NodeId {
        // Keep the existing signature: when the dynamic budget is exhausted,
        // return an id that does not exist so every later operation on it is a
        // silent no-op instead of growing the arena without bound.
        if self.dynamic_nodes >= MAX_DYNAMIC_NODES {
            return NodeId(usize::MAX);
        }
        self.dynamic_nodes += 1;
        let id = NodeId(self.nodes.len());
        self.nodes.push(Node {
            kind: NodeKind::Element {
                tag_name: tag_name.trim().to_ascii_lowercase(),
                attributes: Vec::new(),
            },
            parent: None,
            children: Vec::new(),
        });
        id
    }

    /// Returns true when `ancestor` is `id` or one of its ancestors.
    ///
    /// The walk is bounded by the arena size so that even a pre-existing cycle
    /// cannot turn the check into an infinite loop.
    fn is_ancestor_of(&self, ancestor: NodeId, id: NodeId) -> bool {
        let mut current = Some(id);
        for _ in 0..=self.nodes.len() {
            let Some(node_id) = current else { return false };
            if node_id == ancestor {
                return true;
            }
            current = self.node(node_id).and_then(Node::parent);
        }
        true
    }

    /// Number of nodes on the path from `id` up to (and including) the root.
    fn node_depth(&self, id: NodeId) -> usize {
        let mut depth = 0;
        let mut current = Some(id);
        for _ in 0..=self.nodes.len() {
            let Some(node_id) = current else { return depth };
            depth += 1;
            current = self.node(node_id).and_then(Node::parent);
        }
        depth
    }

    /// Height of the subtree rooted at `id`, counting the root itself. The
    /// walk is iterative and bounded so detached subtrees can be measured
    /// without recursion or an unbounded loop.
    fn subtree_height(&self, id: NodeId) -> usize {
        let mut height = 0;
        let mut stack = vec![(id, 1usize)];
        while let Some((node_id, depth)) = stack.pop() {
            height = height.max(depth);
            let Some(node) = self.node(node_id) else { continue };
            for child in node.children() {
                if stack.len() < self.nodes.len() {
                    stack.push((*child, depth + 1));
                }
            }
        }
        height
    }

    /// Appends a node as the last child of a parent. Returns false when either
    /// node is missing, the child already has a parent, or inserting it would
    /// create a cycle (including appending a node to itself).
    pub fn append_child(&mut self, parent: NodeId, child: NodeId) -> bool {
        if self.nodes.get(parent.index()).is_none() || self.nodes.get(child.index()).is_none() {
            return false;
        }
        if self.nodes[child.0].parent.is_some() {
            return false;
        }
        // `child` must not already contain `parent`; otherwise the new link
        // would close a loop and every recursive DOM walk could hang.
        if self.is_ancestor_of(child, parent) {
            return false;
        }
        // Bound the *final* depth, including the whole detached subtree being
        // attached. Checking only the parent's ancestors would let a deep
        // detached tree push the combined depth past the limit.
        let parent_depth = self.node_depth(parent);
        let child_height = self.subtree_height(child);
        if parent_depth.saturating_add(child_height) > MAX_DOM_DEPTH.saturating_add(1) {
            return false;
        }
        self.nodes[child.0].parent = Some(parent);
        self.nodes[parent.0].children.push(child);
        true
    }

    /// Returns whether an element's `class` attribute contains a class token.
    #[must_use]
    pub fn has_class(&self, id: NodeId, name: &str) -> bool {
        self.attribute(id, "class")
            .is_some_and(|value| value.split_ascii_whitespace().any(|part| part == name))
    }

    /// Adds or removes a class token from an element's `class` attribute.
    pub fn set_class(&mut self, id: NodeId, name: &str, present: bool) {
        let mut classes: Vec<String> = self
            .attribute(id, "class")
            .unwrap_or("")
            .split_ascii_whitespace()
            .map(str::to_owned)
            .collect();
        if present {
            if !classes.iter().any(|class| class == name) {
                classes.push(name.to_owned());
            }
        } else {
            classes.retain(|class| class != name);
        }
        self.set_attribute(id, "class", &classes.join(" "));
    }

    /// Returns `<script>` elements in document order, preserving the relative
    /// order of inline and external scripts.
    #[must_use]
    pub fn script_references(&self) -> Vec<ScriptRef> {
        self.nodes
            .iter()
            .enumerate()
            .filter(|(index, node)| {
                !self.is_in_template(NodeId(*index))
                    && matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "script")
            })
            .filter_map(|(index, node)| {
                let NodeKind::Element { attributes, .. } = node.kind() else { return None };
                if let Some(src) = attributes.iter().find(|a| a.name == "src").map(|a| a.value.as_str()) {
                    if !src.trim().is_empty() {
                        return Some(ScriptRef::External(src.to_owned()));
                    }
                }
                let mut source = String::new();
                self.collect_text(NodeId(index), &mut source);
                (!source.trim().is_empty()).then_some(ScriptRef::Inline(source))
            })
            .collect()
    }

    /// Returns the contents of inline `<script>` elements in document order.
    #[must_use]
    pub fn script_sources(&self) -> Vec<String> {
        self.nodes
            .iter()
            .enumerate()
            .filter(|(index, node)| {
                !self.is_in_template(NodeId(*index))
                    && matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "script")
            })
            .map(|(index, _)| {
                let mut source = String::new();
                self.collect_text(NodeId(index), &mut source);
                source
            })
            .filter(|source| !source.trim().is_empty())
            .collect()
    }

    /// Lists external stylesheets, images, and scripts in document order.
    #[must_use]
    pub fn resource_references(&self) -> Vec<ResourceReference> {
        let mut references = Vec::new();
        for (index, node) in self.nodes.iter().enumerate() {
            if self.is_in_template(NodeId(index)) {
                continue;
            }
            let NodeKind::Element { tag_name, attributes } = &node.kind else {
                continue;
            };
            let attribute = |name: &str| {
                attributes.iter().find(|item| item.name == name).map(|item| item.value.as_str())
            };
            let reference = match tag_name.as_str() {
                "link"
                    if attribute("rel").is_some_and(|rel| {
                        rel.split_ascii_whitespace()
                            .any(|part| part.eq_ignore_ascii_case("stylesheet"))
                    }) =>
                {
                    attribute("href").map(|url| (url, ResourceKind::Stylesheet))
                }
                "img" => attribute("src").map(|url| (url, ResourceKind::Image)),
                "script" => attribute("src").map(|url| (url, ResourceKind::Script)),
                _ => None,
            };
            if let Some((url, kind)) = reference {
                if !url.trim().is_empty() {
                    references.push(ResourceReference { url: url.to_owned(), kind });
                }
            }
        }
        references
    }

    fn append_node(&mut self, parent: NodeId, kind: NodeKind) -> NodeId {
        self.insert_node(parent, None, kind)
    }

    fn insert_node(&mut self, parent: NodeId, before: Option<NodeId>, kind: NodeKind) -> NodeId {
        let id = NodeId(self.nodes.len());
        self.nodes.push(Node { kind, parent: Some(parent), children: Vec::new() });
        let children = &mut self.nodes[parent.0].children;
        match before.and_then(|before| children.iter().position(|child| *child == before)) {
            Some(position) => children.insert(position, id),
            None => children.push(id),
        }
        id
    }

    /// HTML5 foster parenting: content found directly inside a table (outside
    /// cells) is inserted before the table in its parent.
    fn insertion_point(&self, stack: &[NodeId], new_tag: &str) -> (NodeId, Option<NodeId>) {
        if matches!(
            new_tag,
            "table"
                | "caption"
                | "colgroup"
                | "col"
                | "thead"
                | "tbody"
                | "tfoot"
                | "tr"
                | "td"
                | "th"
        ) {
            return (*stack.last().unwrap_or(&self.root), None);
        }
        let Some(table_index) = stack.iter().rposition(|id| {
            matches!(&self.nodes[id.0].kind, NodeKind::Element { tag_name, .. } if tag_name == "table")
        }) else {
            return (*stack.last().unwrap_or(&self.root), None);
        };
        let inside_cell = stack[table_index + 1..].iter().any(|id| {
            matches!(
                &self.nodes[id.0].kind,
                NodeKind::Element { tag_name, .. }
                    if tag_name == "td" || tag_name == "th" || tag_name == "caption"
            )
        });
        if inside_cell {
            return (*stack.last().unwrap_or(&self.root), None);
        }
        let table = stack[table_index];
        match self.nodes[table.0].parent() {
            Some(parent) if parent != table => (parent, Some(table)),
            _ => (*stack.last().unwrap_or(&self.root), None),
        }
    }

    fn build_tree(&mut self) {
        // Move the source out temporarily so tree construction can mutate the
        // arena while borrowing markup slices without cloning the document.
        let source = std::mem::take(&mut self.source);
        TreeBuilder::new(self).build(&source, 0);
        self.source = source;
    }

    fn append_text(&mut self, parent: NodeId, text: &str) {
        if text.is_empty() {
            return;
        }
        // <script> and <style> are raw-text elements: their content must reach
        // the JS/CSS consumers unchanged, including any `&...;` sequences.
        let raw_text = matches!(
            self.nodes[parent.0].kind(),
            NodeKind::Element { tag_name, .. } if tag_name == "script" || tag_name == "style"
        );
        let decoded = if raw_text { text.to_owned() } else { decode_entities(text) };
        if !decoded.is_empty() {
            self.append_node(parent, NodeKind::Text(decoded));
        }
    }

    /// Returns whether a node lives inside a template's inert content tree.
    #[must_use]
    pub fn is_in_template(&self, id: NodeId) -> bool {
        let mut current = self.node(id).and_then(|node| node.parent());
        while let Some(parent) = current {
            if matches!(self.node(parent).map(|node| node.kind()), Some(NodeKind::TemplateContent))
            {
                return true;
            }
            current = self.node(parent).and_then(|node| node.parent());
        }
        false
    }

    fn collect_text(&self, id: NodeId, output: &mut String) {
        let Some(node) = self.node(id) else {
            return;
        };
        if let NodeKind::Text(text) = &node.kind {
            output.push_str(text);
        }
        for child in &node.children {
            self.collect_text(*child, output);
        }
    }
}

/// Upper bound on the open-element stack. Real markup is shallow; a cap keeps
/// adversarial input from growing the arena without limit.
const MAX_STACK_DEPTH: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InsertionMode {
    Initial,
    BeforeHead,
    InHead,
    AfterHead,
    InBody,
    AfterBody,
}

/// Elements that are processed with the "in head" rules even when a `<body>`
/// is already open.
fn is_head_element(name: &str) -> bool {
    matches!(
        name,
        "base"
            | "basefont"
            | "bgsound"
            | "link"
            | "meta"
            | "title"
            | "style"
            | "script"
            | "noframes"
            | "template"
    )
}

/// Elements whose content is text rather than markup. RCDATA elements still
/// decode character references; the others keep their content verbatim.
fn raw_text_end_tag(name: &str) -> Option<&'static str> {
    match name {
        "script" => Some("script"),
        "style" => Some("style"),
        "title" => Some("title"),
        "textarea" => Some("textarea"),
        "xmp" => Some("xmp"),
        "iframe" => Some("iframe"),
        "noembed" => Some("noembed"),
        "noframes" => Some("noframes"),
        _ => None,
    }
}

fn is_rcdata_element(name: &str) -> bool {
    matches!(name, "title" | "textarea")
}

/// Elements that stop formatting-element adoption and affect implied end tags.
fn is_special_element(name: &str) -> bool {
    matches!(
        name,
        "address"
            | "applet"
            | "area"
            | "article"
            | "aside"
            | "blockquote"
            | "body"
            | "br"
            | "button"
            | "caption"
            | "center"
            | "col"
            | "colgroup"
            | "dd"
            | "details"
            | "dir"
            | "div"
            | "dl"
            | "dt"
            | "fieldset"
            | "figcaption"
            | "figure"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "head"
            | "header"
            | "hgroup"
            | "hr"
            | "html"
            | "iframe"
            | "img"
            | "input"
            | "li"
            | "link"
            | "main"
            | "menu"
            | "meta"
            | "nav"
            | "noembed"
            | "noframes"
            | "object"
            | "ol"
            | "p"
            | "param"
            | "pre"
            | "script"
            | "section"
            | "select"
            | "source"
            | "style"
            | "summary"
            | "table"
            | "tbody"
            | "td"
            | "textarea"
            | "tfoot"
            | "th"
            | "thead"
            | "title"
            | "tr"
            | "track"
            | "ul"
            | "wbr"
            | "xmp"
    )
}

/// Inline elements that HTML5 tracks as active formatting elements.
fn is_formatting_element(name: &str) -> bool {
    matches!(
        name,
        "a" | "b"
            | "big"
            | "code"
            | "em"
            | "font"
            | "i"
            | "nobr"
            | "s"
            | "small"
            | "strike"
            | "strong"
            | "tt"
            | "u"
    )
}

/// Finds the closing tag for a raw-text element, returning the end of its text
/// and the end of the closing tag itself.
fn find_raw_text_end(source: &str, from: usize, tag: &str) -> Option<(usize, usize)> {
    let bytes = source.as_bytes();
    let mut index = from;
    while index + 2 < bytes.len() {
        if bytes[index] != b'<' || bytes[index + 1] != b'/' {
            index += 1;
            continue;
        }
        let name_start = index + 2;
        let mut name_end = name_start;
        while name_end < bytes.len() && bytes[name_end].is_ascii_alphanumeric() {
            name_end += 1;
        }
        let matches =
            source.get(name_start..name_end).is_some_and(|name| name.eq_ignore_ascii_case(tag))
                && matches!(bytes.get(name_end), Some(b' ' | b'\t' | b'\n' | b'\r' | b'>' | b'/'));
        if matches {
            let close_end = name_end + find_tag_end(source, name_end)? + 1;
            return Some((index, close_end));
        }
        index += 1;
    }
    None
}

/// Stateful HTML tree construction over the arena DOM.
struct TreeBuilder<'a> {
    document: &'a mut HtmlDocument,
    root: NodeId,
    stack: Vec<NodeId>,
    mode: InsertionMode,
    html: Option<NodeId>,
    head: Option<NodeId>,
    body: Option<NodeId>,
    active_formatting: Vec<NodeId>,
}

impl<'a> TreeBuilder<'a> {
    fn new(document: &'a mut HtmlDocument) -> Self {
        let root = document.root;
        Self {
            document,
            root,
            stack: vec![root],
            mode: InsertionMode::Initial,
            html: None,
            head: None,
            body: None,
            active_formatting: Vec::new(),
        }
    }

    fn tag(&self, id: NodeId) -> Option<&str> {
        match self.document.nodes.get(id.0).map(|node| node.kind()) {
            Some(NodeKind::Element { tag_name, .. }) => Some(tag_name.as_str()),
            _ => None,
        }
    }

    fn current(&self) -> NodeId {
        self.stack.last().copied().unwrap_or(self.root)
    }

    /// Inserts a node using the current insertion point, including foster
    /// parenting for stray table content.
    fn insert(&mut self, kind: NodeKind) -> NodeId {
        let tag = match &kind {
            NodeKind::Element { tag_name, .. } => tag_name.as_str(),
            _ => "",
        };
        let (parent, before) = self.document.insertion_point(&self.stack, tag);
        if let Some(before) = before {
            self.document.insert_node(parent, Some(before), kind)
        } else {
            self.document.append_node(self.current(), kind)
        }
    }

    fn push(&mut self, id: NodeId, tag: &str, self_closing: bool) {
        if !self_closing && !is_void_element(tag) && self.stack.len() < MAX_STACK_DEPTH {
            self.stack.push(id);
        }
    }

    fn pop_element(&mut self, id: NodeId) {
        if let Some(position) = self.stack.iter().rposition(|open| *open == id) {
            self.stack.truncate(position);
        }
    }

    /// Reopens formatting elements that were implicitly closed by a block
    /// boundary, so later text keeps the intended inline styling.
    fn detached_element(&mut self, tag: &str, attributes: Vec<Attribute>) -> NodeId {
        let id = NodeId(self.document.nodes.len());
        self.document.nodes.push(Node {
            kind: NodeKind::Element { tag_name: tag.to_owned(), attributes },
            parent: None,
            children: Vec::new(),
        });
        id
    }

    fn adopt(&mut self, node: NodeId, parent: NodeId, before: Option<NodeId>) {
        if let Some(old) = self.document.nodes[node.0].parent {
            self.document.nodes[old.0].children.retain(|child| *child != node);
        }
        self.document.nodes[node.0].parent = Some(parent);
        let children = &mut self.document.nodes[parent.0].children;
        match before.and_then(|before| children.iter().position(|child| *child == before)) {
            Some(position) => children.insert(position, node),
            None => children.push(node),
        }
    }

    fn clone_formatting_element(&mut self, id: NodeId) -> NodeId {
        let (tag, attributes) = match &self.document.nodes[id.0].kind {
            NodeKind::Element { tag_name, attributes } => (tag_name.clone(), attributes.clone()),
            _ => (String::new(), Vec::new()),
        };
        self.detached_element(&tag, attributes)
    }

    /// HTML5 adoption agency for a formatting end tag whose element is above a
    /// special block in the stack. Clones the formatting element around the
    /// block's content so formatting survives past the misnested end tag.
    fn adoption_agency(&mut self, name: &str) -> bool {
        let Some(formatting_pos) =
            self.active_formatting.iter().rposition(|id| self.tag(*id) == Some(name))
        else {
            return false;
        };
        let formatting = self.active_formatting[formatting_pos];
        let Some(formatting_stack) = self.stack.iter().rposition(|id| *id == formatting) else {
            self.active_formatting.remove(formatting_pos);
            return true;
        };
        if formatting_stack + 1 == self.stack.len() {
            self.stack.pop();
            self.active_formatting.remove(formatting_pos);
            return true;
        }
        let furthest_rel = self.stack[formatting_stack + 1..]
            .iter()
            .position(|id| self.tag(*id).is_some_and(is_special_element));
        let Some(furthest_rel) = furthest_rel else {
            self.stack.truncate(formatting_stack);
            self.active_formatting.remove(formatting_pos);
            return true;
        };
        let furthest_stack = formatting_stack + 1 + furthest_rel;
        let furthest = self.stack[furthest_stack];

        let children: Vec<NodeId> = self.document.nodes[furthest.0].children.clone();
        let clone = self.clone_formatting_element(formatting);
        for child in children {
            self.adopt(child, clone, None);
        }
        self.adopt(clone, furthest, None);

        self.active_formatting.remove(formatting_pos);
        self.active_formatting.insert(formatting_pos, clone);
        self.stack.remove(formatting_stack);
        let insert_at =
            self.stack.iter().position(|id| *id == furthest).unwrap_or(self.stack.len());
        self.stack.insert(insert_at + 1, clone);
        true
    }

    fn reconstruct_formatting(&mut self) {
        self.active_formatting.retain(|id| self.document.nodes.get(id.0).is_some());
        let mut index = 0;
        while index < self.active_formatting.len() {
            let original = self.active_formatting[index];
            if self.stack.contains(&original) {
                index += 1;
                continue;
            }
            let tag = self.tag(original).unwrap_or("").to_owned();
            let attributes = match &self.document.nodes[original.0].kind {
                NodeKind::Element { attributes, .. } => attributes.clone(),
                _ => Vec::new(),
            };
            let clone = self.insert(NodeKind::Element { tag_name: tag.clone(), attributes });
            self.push(clone, &tag, false);
            self.active_formatting[index] = clone;
            index += 1;
        }
    }

    fn merge_attributes(&mut self, id: NodeId, attributes: Vec<Attribute>) {
        let NodeKind::Element { attributes: existing, .. } =
            &mut self.document.nodes.get_mut(id.0).expect("element").kind
        else {
            return;
        };
        for attribute in attributes {
            if !existing.iter().any(|current| current.name == attribute.name) {
                existing.push(attribute);
            }
        }
    }

    fn ensure_html(&mut self, attributes: Vec<Attribute>) {
        if let Some(id) = self.html {
            self.merge_attributes(id, attributes);
            return;
        }
        let id = self
            .document
            .append_node(self.root, NodeKind::Element { tag_name: "html".to_owned(), attributes });
        self.html = Some(id);
        self.stack = vec![self.root, id];
    }

    fn ensure_head(&mut self) {
        if self.head.is_some() {
            return;
        }
        self.ensure_html(Vec::new());
        let id =
            self.insert(NodeKind::Element { tag_name: "head".to_owned(), attributes: Vec::new() });
        self.head = Some(id);
        self.push(id, "head", false);
    }

    fn ensure_body(&mut self, attributes: Vec<Attribute>) {
        if let Some(id) = self.body {
            self.merge_attributes(id, attributes);
            return;
        }
        self.ensure_html(Vec::new());
        while self.stack.len() > 2 {
            self.stack.pop();
        }
        let id = self.insert(NodeKind::Element { tag_name: "body".to_owned(), attributes });
        self.body = Some(id);
        self.push(id, "body", false);
    }

    fn close_head(&mut self) {
        self.ensure_head();
        while self.stack.len() > 2 {
            self.stack.pop();
        }
        self.mode = InsertionMode::AfterHead;
    }

    fn insert_text(&mut self, text: &str, raw: bool) {
        let text = if raw { text.to_owned() } else { decode_entities(text) };
        if text.is_empty() {
            return;
        }
        let (parent, before) = self.document.insertion_point(&self.stack, "");
        if before.is_some() && !text.trim().is_empty() {
            self.document.insert_node(parent, before, NodeKind::Text(text));
        } else {
            self.document.append_text(self.current(), &text);
        }
    }

    fn comment(&mut self, value: String) {
        self.insert(NodeKind::Comment(value));
    }

    /// Inserts a head-only element and returns its node id. Outside the "in
    /// head" mode the head is pushed temporarily so the element lands there.
    fn insert_head_element(&mut self, tag: &str, attributes: Vec<Attribute>) -> NodeId {
        self.ensure_head();
        let head = self.head.expect("head");
        let temporary = !self.stack.contains(&head);
        if temporary {
            self.stack.push(head);
        }
        let id = self.insert(NodeKind::Element { tag_name: tag.to_owned(), attributes });
        if temporary {
            self.stack.pop();
        }
        id
    }

    fn build(&mut self, source: &str, mut cursor: usize) {
        while cursor < source.len() {
            let Some(relative_open) = source[cursor..].find('<') else {
                self.text(&source[cursor..]);
                break;
            };
            let open = cursor + relative_open;
            if open > cursor {
                self.text(&source[cursor..open]);
            }
            if source[open..].starts_with("<!--") {
                if let Some(relative_end) = source[open + 4..].find("-->") {
                    let end = open + 4 + relative_end;
                    self.comment(source[open + 4..end].to_owned());
                    cursor = end + 3;
                    continue;
                }
                self.text(&source[open..]);
                break;
            }
            if source[open..].starts_with("<!") || source[open..].starts_with("<?") {
                // Doctypes and processing instructions have no DOM node here.
                if let Some(relative_end) = find_tag_end(source, open + 2) {
                    cursor = open + 2 + relative_end + 1;
                } else {
                    break;
                }
                continue;
            }
            let Some(relative_end) = find_tag_end(source, open + 1) else {
                self.text(&source[open..]);
                break;
            };
            let end = open + 1 + relative_end;
            let token = source[open + 1..end].trim();
            if let Some(closing_name) = token.strip_prefix('/') {
                let name = parse_name(closing_name);
                self.end_tag(&name);
                cursor = end + 1;
                continue;
            }
            if token.is_empty() {
                cursor = end + 1;
                continue;
            }
            let (name, attributes, self_closing) = parse_start_tag(token);
            if name.is_empty() {
                cursor = end + 1;
                continue;
            }
            if name == "template" && !self_closing {
                // Template content lives in a separate inert subtree.
                let template = self.insert(NodeKind::Element { tag_name: name, attributes });
                let content = self.document.append_node(template, NodeKind::TemplateContent);
                // A template adds two stack entries (the element and its inert
                // content root). Apply the same cap as `push()` here so nested
                // templates cannot bypass MAX_STACK_DEPTH.
                if self.stack.len() + 2 <= MAX_STACK_DEPTH {
                    self.stack.push(template);
                    self.stack.push(content);
                }
                cursor = end + 1;
                continue;
            }
            match self.start_tag(&name, attributes, self_closing) {
                Some(id) if raw_text_end_tag(&name).is_some() => {
                    cursor = self.raw_text(source, end + 1, id, &name);
                    self.pop_element(id);
                }
                Some(_) if name == "plaintext" => {
                    if end + 1 < source.len() {
                        self.insert_text(&source[end + 1..], true);
                    }
                    break;
                }
                _ => cursor = end + 1,
            }
        }
    }

    fn text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        match self.mode {
            InsertionMode::Initial | InsertionMode::BeforeHead if text.trim().is_empty() => {}
            InsertionMode::Initial => {
                self.ensure_html(Vec::new());
                self.mode = InsertionMode::BeforeHead;
                self.text(text);
            }
            InsertionMode::BeforeHead if !text.trim().is_empty() => {
                self.ensure_head();
                self.close_head();
                self.text(text);
            }
            InsertionMode::AfterHead if !text.trim().is_empty() => {
                self.ensure_body(Vec::new());
                self.mode = InsertionMode::InBody;
                self.text(text);
            }
            InsertionMode::AfterBody => {
                self.mode = InsertionMode::InBody;
                self.text(text);
            }
            _ => {
                self.reconstruct_formatting();
                let raw =
                    self.tag(self.current()).is_some_and(|tag| raw_text_end_tag(tag).is_some());
                self.insert_text(text, raw);
            }
        }
    }

    fn raw_text(&mut self, source: &str, from: usize, element: NodeId, tag: &str) -> usize {
        let element_tag = self.tag(element).unwrap_or(tag).to_owned();
        let Some((text_end, close_end)) = find_raw_text_end(source, from, tag) else {
            if from < source.len() {
                let content = &source[from..];
                if is_rcdata_element(&element_tag) {
                    let decoded = decode_entities(content);
                    self.document.append_text(element, &decoded);
                } else {
                    self.document.append_text(element, content);
                }
            }
            return source.len();
        };
        if text_end > from {
            let content = &source[from..text_end];
            if is_rcdata_element(&element_tag) {
                self.document.append_text(element, &decode_entities(content));
            } else {
                self.document.append_text(element, content);
            }
        }
        close_end
    }

    fn start_tag(
        &mut self,
        name: &str,
        attributes: Vec<Attribute>,
        self_closing: bool,
    ) -> Option<NodeId> {
        match self.mode {
            InsertionMode::Initial => {
                self.ensure_html(attributes.clone());
                self.mode = InsertionMode::BeforeHead;
                if name == "html" { None } else { self.start_tag(name, attributes, self_closing) }
            }
            InsertionMode::BeforeHead => {
                if name == "head" {
                    self.ensure_head();
                    self.merge_attributes(self.head.expect("head"), attributes);
                    self.mode = InsertionMode::InHead;
                    None
                } else {
                    self.ensure_head();
                    self.close_head();
                    self.start_tag(name, attributes, self_closing)
                }
            }
            InsertionMode::InHead => {
                if name == "html" {
                    self.ensure_html(attributes);
                    return None;
                }
                if name == "head" {
                    return None;
                }
                if is_head_element(name) {
                    return Some(self.insert_head_element(name, attributes));
                }
                self.close_head();
                self.start_tag(name, attributes, self_closing)
            }
            InsertionMode::AfterHead => {
                if name == "html" {
                    self.ensure_html(attributes);
                    return None;
                }
                if name == "head" {
                    return None;
                }
                if name == "body" {
                    self.ensure_body(attributes);
                    self.mode = InsertionMode::InBody;
                    return None;
                }
                if is_head_element(name) {
                    return Some(self.insert_head_element(name, attributes));
                }
                self.ensure_body(Vec::new());
                self.mode = InsertionMode::InBody;
                self.start_tag(name, attributes, self_closing)
            }
            InsertionMode::InBody | InsertionMode::AfterBody => {
                self.mode = InsertionMode::InBody;
                match name {
                    "html" => {
                        self.ensure_html(attributes);
                        return None;
                    }
                    "body" => {
                        self.ensure_body(attributes);
                        return None;
                    }
                    "head" => return None,
                    name if is_head_element(name) => {
                        // In-body head elements (scripts, styles) belong to
                        // the current node, which is normally body here.
                        let id = self
                            .insert(NodeKind::Element { tag_name: name.to_owned(), attributes });
                        return Some(id);
                    }
                    _ => {}
                }
                // A block-level start tag closes an open paragraph even when
                // formatting elements sit above it; those are then rebuilt.
                if impliedly_closes_paragraph(name) {
                    if let Some(position) =
                        self.stack.iter().rposition(|id| self.tag(*id) == Some("p"))
                    {
                        self.stack.truncate(position);
                    }
                }
                close_implied_elements(&mut self.stack, &self.document.nodes, name);
                let id = self.insert(NodeKind::Element { tag_name: name.to_owned(), attributes });
                self.push(id, name, self_closing);
                if is_formatting_element(name) && !self_closing {
                    self.active_formatting.push(id);
                }
                Some(id)
            }
        }
    }

    fn end_tag(&mut self, name: &str) {
        match self.mode {
            InsertionMode::InHead if name == "head" => {
                self.close_head();
                return;
            }
            InsertionMode::InBody | InsertionMode::AfterBody if name == "body" => {
                self.mode = InsertionMode::AfterBody;
                return;
            }
            InsertionMode::InBody | InsertionMode::AfterBody if name == "html" => {
                self.mode = InsertionMode::AfterBody;
                return;
            }
            _ => {}
        }
        if is_formatting_element(name) && self.adoption_agency(name) {
            return;
        }
        if is_formatting_element(name) {
            if let Some(position) =
                self.active_formatting.iter().rposition(|id| self.tag(*id) == Some(name))
            {
                let element = self.active_formatting.remove(position);
                if let Some(stack_position) = self.stack.iter().rposition(|open| *open == element) {
                    self.stack.truncate(stack_position);
                }
                if self.stack.is_empty() {
                    self.stack.push(self.root);
                }
                return;
            }
        }
        if let Some(position) = self.stack.iter().rposition(|id| self.tag(*id) == Some(name)) {
            self.stack.truncate(position);
        }
        if self.stack.is_empty() {
            self.stack.push(self.root);
        }
    }
}

fn find_tag_end(source: &str, start: usize) -> Option<usize> {
    let mut quote = None;
    for (offset, byte) in source.as_bytes()[start..].iter().enumerate() {
        match (*byte, quote) {
            (b'\'' | b'"', None) => quote = Some(*byte),
            (byte, Some(current)) if byte == current => quote = None,
            (b'>', None) => return Some(offset),
            _ => {}
        }
    }
    None
}

fn parse_name(input: &str) -> String {
    input
        .trim_start()
        .split(|character: char| {
            character.is_ascii_whitespace() || character == '/' || character == '>'
        })
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn parse_start_tag(token: &str) -> (String, Vec<Attribute>, bool) {
    let bytes = token.as_bytes();
    let mut index = 0;
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    let name_start = index;
    while index < bytes.len() && !bytes[index].is_ascii_whitespace() && bytes[index] != b'/' {
        index += 1;
    }
    let name = String::from_utf8_lossy(&bytes[name_start..index]).to_ascii_lowercase();
    let mut attributes = Vec::new();
    let mut self_closing = false;
    while index < bytes.len() {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() {
            break;
        }
        if bytes[index] == b'/' {
            self_closing = true;
            break;
        }
        let attr_start = index;
        while index < bytes.len()
            && !bytes[index].is_ascii_whitespace()
            && bytes[index] != b'='
            && bytes[index] != b'/'
        {
            index += 1;
        }
        if attr_start == index {
            index += 1;
            continue;
        }
        let attr_name = String::from_utf8_lossy(&bytes[attr_start..index]).to_ascii_lowercase();
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let mut value = String::new();
        if index < bytes.len() && bytes[index] == b'=' {
            index += 1;
            while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            if index < bytes.len() && (bytes[index] == b'\'' || bytes[index] == b'"') {
                let quote = bytes[index];
                index += 1;
                let value_start = index;
                while index < bytes.len() && bytes[index] != quote {
                    index += 1;
                }
                value = decode_attribute(&String::from_utf8_lossy(&bytes[value_start..index]));
                if index < bytes.len() {
                    index += 1;
                }
            } else {
                let value_start = index;
                while index < bytes.len() && !bytes[index].is_ascii_whitespace() {
                    index += 1;
                }
                value = decode_attribute(&String::from_utf8_lossy(&bytes[value_start..index]));
            }
        }
        if !attributes.iter().any(|attribute: &Attribute| attribute.name == attr_name) {
            attributes.push(Attribute { name: attr_name, value });
        }
    }
    (name, attributes, self_closing)
}

/// Pops open elements that an HTML start tag implicitly closes.
///
/// This is a small, explicit subset of the HTML5 tree builder: omitted end tags
/// for paragraphs, list items, table rows/cells and a few similar elements.
fn close_implied_elements(stack: &mut Vec<NodeId>, nodes: &[Node], new_tag: &str) {
    while let Some(&top) = stack.last() {
        let NodeKind::Element { tag_name, .. } = &nodes[top.0].kind else {
            break;
        };
        if !impliedly_closed(tag_name, new_tag) {
            break;
        }
        stack.pop();
    }
}

fn impliedly_closes_paragraph(new_tag: &str) -> bool {
    impliedly_closed("p", new_tag)
}

fn impliedly_closed(open_tag: &str, new_tag: &str) -> bool {
    match open_tag {
        "p" => matches!(
            new_tag,
            "address"
                | "article"
                | "aside"
                | "blockquote"
                | "div"
                | "dl"
                | "fieldset"
                | "footer"
                | "form"
                | "h1"
                | "h2"
                | "h3"
                | "h4"
                | "h5"
                | "h6"
                | "header"
                | "hgroup"
                | "hr"
                | "main"
                | "menu"
                | "nav"
                | "ol"
                | "p"
                | "pre"
                | "search"
                | "section"
                | "table"
                | "ul"
        ),
        "li" => new_tag == "li",
        "dt" | "dd" => new_tag == "dt" || new_tag == "dd",
        "tr" => new_tag == "tr",
        "td" | "th" => new_tag == "td" || new_tag == "th",
        "option" => new_tag == "option",
        "thead" | "tbody" | "tfoot" => {
            new_tag == "thead" || new_tag == "tbody" || new_tag == "tfoot"
        }
        _ => false,
    }
}

fn decode_entities(input: &str) -> String {
    decode_entities_inner(input, false)
}

/// Attribute values only decode references terminated by a semicolon, so an
/// unescaped `&copy=1` query string survives intact.
fn decode_attribute(input: &str) -> String {
    decode_entities_inner(input, true)
}

fn decode_entities_inner(input: &str, require_semicolon: bool) -> String {
    let Some(first) = input.find('&') else {
        return input.to_owned();
    };
    let mut output = String::with_capacity(input.len());
    output.push_str(&input[..first]);
    let mut rest = &input[first..];

    while let Some(offset) = rest.find('&') {
        output.push_str(&rest[..offset]);
        rest = &rest[offset..];
        let after = &rest[1..];
        if let Some((decoded, consumed)) = numeric_reference(after) {
            output.push_str(&decoded);
            rest = &after[consumed..];
            continue;
        }
        if let Some((replacement, consumed)) = named_reference(after, require_semicolon) {
            output.push_str(replacement);
            rest = &after[consumed..];
            continue;
        }
        output.push('&');
        rest = after;
    }
    output.push_str(rest);
    output
}

/// Longest-match lookup in the full HTML5 named reference table. Legacy
/// references without a semicolon are included, so `&copy ` decodes while
/// `&copycat` stays literal unless a longer reference matches.
fn named_reference(after: &str, require_semicolon: bool) -> Option<(&'static str, usize)> {
    let mut lengths: Vec<usize> = after.char_indices().map(|(index, _)| index).collect();
    lengths.push(after.len());
    for length in lengths.into_iter().rev().filter(|length| *length > 0) {
        let name = &after[..length];
        if require_semicolon && !name.ends_with(';') {
            continue;
        }
        if let Ok(position) =
            entities::NAMED_REFERENCES.binary_search_by(|(candidate, _)| (*candidate).cmp(name))
        {
            return Some((entities::NAMED_REFERENCES[position].1, length));
        }
    }
    None
}

/// Numeric references require a trailing semicolon. Code points in the
/// Windows-1252 control range are remapped as HTML5 prescribes.
fn numeric_reference(after: &str) -> Option<(String, usize)> {
    let digits = after.strip_prefix('#')?;
    let (radix, digits) = match digits.as_bytes().first()? {
        b'x' | b'X' => (16, &digits[1..]),
        _ => (10, digits),
    };
    let semicolon = digits.find(';')?;
    let code = u32::from_str_radix(&digits[..semicolon], radix).ok()?;
    let character = match code {
        0 => '\u{fffd}',
        0x80 => '\u{20ac}',
        0x82 => '\u{201a}',
        0x83 => '\u{0192}',
        0x84 => '\u{201e}',
        0x85 => '\u{2026}',
        0x86 => '\u{2020}',
        0x87 => '\u{2021}',
        0x88 => '\u{02c6}',
        0x89 => '\u{2030}',
        0x8a => '\u{0160}',
        0x8b => '\u{2039}',
        0x8c => '\u{0152}',
        0x8e => '\u{017d}',
        0x91 => '\u{2018}',
        0x92 => '\u{2019}',
        0x93 => '\u{201c}',
        0x94 => '\u{201d}',
        0x95 => '\u{2022}',
        0x96 => '\u{2013}',
        0x97 => '\u{2014}',
        0x98 => '\u{02dc}',
        0x99 => '\u{2122}',
        0x9a => '\u{0161}',
        0x9b => '\u{203a}',
        0x9c => '\u{0153}',
        0x9e => '\u{017e}',
        0x9f => '\u{0178}',
        _ => char::from_u32(code)?,
    };
    let consumed = after.find(';')? + 1;
    Some((character.to_string(), consumed))
}

fn is_void_element(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

/// Kind of external resource referenced by a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    Stylesheet,
    Image,
    Script,
}

impl ResourceKind {
    const fn network_type(self) -> ResourceType {
        match self {
            Self::Stylesheet => ResourceType::Stylesheet,
            Self::Image => ResourceType::Image,
            Self::Script => ResourceType::Script,
        }
    }

    const fn max_bytes(self) -> usize {
        match self {
            Self::Stylesheet => MAX_STYLESHEET_BYTES,
            Self::Image => MAX_IMAGE_BYTES,
            Self::Script => MAX_SCRIPT_BYTES,
        }
    }
}

/// A `<script>` element in document order: either inline source or an external
/// URL to resolve against the loaded resources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptRef {
    Inline(String),
    External(String),
}

/// A URL reference extracted from the DOM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceReference {
    pub url: String,
    pub kind: ResourceKind,
}

/// A successfully fetched external resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedResource {
    pub url: Url,
    pub kind: ResourceKind,
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
}

/// A resource failure is isolated: one blocked or unavailable subresource does
/// not discard the already parsed document or prevent other resources loading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedResource {
    pub reference: ResourceReference,
    pub error: String,
}

/// Parsed document plus the external resources fetched through HttpTransport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedDocument {
    pub url: Url,
    pub document: HtmlDocument,
    pub resources: Vec<LoadedResource>,
    pub failed_resources: Vec<FailedResource>,
}

/// Errors that prevent the main document from being loaded.
#[derive(Debug)]
pub enum DocumentLoadError {
    InvalidUrl(url::ParseError),
    Transport(HttpTransportError),
    Io(io::Error),
    HttpStatus(u16),
    TooLarge { limit: usize },
    InvalidUtf8,
}

impl fmt::Display for DocumentLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUrl(error) => write!(f, "invalid document URL: {error}"),
            Self::Transport(error) => write!(f, "document transport failed: {error}"),
            Self::Io(error) => write!(f, "could not read document response: {error}"),
            Self::HttpStatus(status) => write!(f, "document request returned HTTP status {status}"),
            Self::TooLarge { limit } => write!(f, "document exceeded the {limit}-byte limit"),
            Self::InvalidUtf8 => f.write_str("HTML document is not valid UTF-8"),
        }
    }
}

impl Error for DocumentLoadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidUrl(error) => Some(error),
            Self::Transport(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::HttpStatus(_) | Self::TooLarge { .. } | Self::InvalidUtf8 => None,
        }
    }
}

/// Limits applied to all resources downloaded for a single page.
///
/// The defaults are deliberately generous enough for real pages while keeping
/// the total work bounded. Callers may tighten them with
/// [`DocumentLoader::with_limits`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceLimits {
    /// Maximum cumulative bytes, including the main HTML document.
    pub max_total_bytes: usize,
    /// Maximum number of distinct subresources (CSS, scripts and images).
    pub max_resources: usize,
}

impl ResourceLimits {
    pub const DEFAULT: Self =
        Self { max_total_bytes: MAX_TOTAL_RESOURCE_BYTES, max_resources: MAX_TOTAL_RESOURCES };
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Per-page accounting shared by both reference-fetching passes.
#[derive(Debug)]
struct ResourceBudget {
    limits: ResourceLimits,
    used_bytes: usize,
    fetched: usize,
    downloaded: HashSet<String>,
}

/// Decision returned when a reference is considered for download.
enum Admission {
    Fetch,
    /// The resolved URL was already downloaded; skip without reporting a failure.
    Duplicate,
    Rejected(String),
}

impl ResourceBudget {
    fn new(document_bytes: usize, limits: ResourceLimits) -> Self {
        Self { limits, used_bytes: document_bytes, fetched: 0, downloaded: HashSet::new() }
    }

    fn remaining_bytes(&self) -> usize {
        self.limits.max_total_bytes.saturating_sub(self.used_bytes)
    }

    fn admit(&mut self, url: &str) -> Admission {
        if self.downloaded.contains(url) {
            return Admission::Duplicate;
        }
        if self.fetched >= self.limits.max_resources {
            return Admission::Rejected(format!(
                "resource count limit ({}) reached",
                self.limits.max_resources
            ));
        }
        if self.remaining_bytes() == 0 {
            return Admission::Rejected(format!(
                "page byte budget ({}) exhausted",
                self.limits.max_total_bytes
            ));
        }
        self.downloaded.insert(url.to_owned());
        self.fetched += 1;
        Admission::Fetch
    }

    fn record(&mut self, bytes: usize) {
        self.used_bytes = self.used_bytes.saturating_add(bytes);
    }
}

/// Loads documents and their external resources using one guarded transport.
///
/// This type deliberately has no reqwest client and no alternate networking
/// implementation. Its only network capability is the supplied HttpTransport.
pub struct DocumentLoader {
    transport: HttpTransport,
    limits: ResourceLimits,
}

impl DocumentLoader {
    #[must_use]
    pub const fn new(transport: HttpTransport) -> Self {
        Self::with_limits(transport, ResourceLimits::DEFAULT)
    }

    /// Builds a loader with a custom per-page resource budget.
    #[must_use]
    pub const fn with_limits(transport: HttpTransport, limits: ResourceLimits) -> Self {
        Self { transport, limits }
    }

    /// Fetches, parses, then loads CSS, image, and script references.
    pub fn load(&self, document_url: &str) -> Result<LoadedDocument, DocumentLoadError> {
        let url = Url::parse(document_url).map_err(DocumentLoadError::InvalidUrl)?;
        let response = self
            .transport
            .execute(
                "GET",
                url.as_str(),
                url.as_str(),
                ResourceType::Document,
                HeaderMap::new(),
                None,
            )
            .map_err(DocumentLoadError::Transport)?;
        if !response.status().is_success() {
            return Err(DocumentLoadError::HttpStatus(response.status().as_u16()));
        }
        // A custom page budget also caps the main document: never read (and
        // allocate) more than the caller allowed, even for the HTML itself.
        let document_limit = MAX_DOCUMENT_BYTES.min(self.limits.max_total_bytes);
        let source = read_bounded(response, document_limit).map_err(|error| match error {
            BoundedReadError::Io { error, .. } => DocumentLoadError::Io(error),
            BoundedReadError::TooLarge { .. } => {
                DocumentLoadError::TooLarge { limit: document_limit }
            }
        })?;
        let source = String::from_utf8(source).map_err(|_| DocumentLoadError::InvalidUtf8)?;
        let document_bytes = source.len();
        let document = HtmlDocument::parse(source);
        let resource_base = document
            .base_href()
            .and_then(|href| url.join(href.trim()).ok())
            .filter(|base| matches!(base.scheme(), "http" | "https"))
            .unwrap_or_else(|| url.clone());
        let mut resources = Vec::new();
        let mut failed_resources = Vec::new();
        // One budget for the whole page: both reference passes, including CSS
        // url() images, draw from the same byte and resource allowances.
        let mut budget = ResourceBudget::new(document_bytes, self.limits);
        let references = document.resource_references();
        self.fetch_references(
            &references,
            &resource_base,
            &url,
            &mut resources,
            &mut failed_resources,
            &mut budget,
        );

        // CSS `url()` references can only be known after stylesheets arrive.
        // Collect them from inline and fetched stylesheets, then load them
        // through the same shielded transport as every other subresource.
        let mut css_urls = Vec::new();
        for source in document.inline_stylesheets() {
            extract_css_urls(&source, &mut css_urls);
        }
        for resource in resources.iter().filter(|item| item.kind == ResourceKind::Stylesheet) {
            if let Ok(source) = std::str::from_utf8(&resource.bytes) {
                extract_css_urls(source, &mut css_urls);
            }
        }
        let css_references: Vec<ResourceReference> = css_urls
            .into_iter()
            .map(|url| ResourceReference { url, kind: ResourceKind::Image })
            .collect();
        self.fetch_references(
            &css_references,
            &resource_base,
            &url,
            &mut resources,
            &mut failed_resources,
            &mut budget,
        );

        Ok(LoadedDocument { url, document, resources, failed_resources })
    }

    fn fetch_references(
        &self,
        references: &[ResourceReference],
        resource_base: &Url,
        document_url: &Url,
        resources: &mut Vec<LoadedResource>,
        failed_resources: &mut Vec<FailedResource>,
        budget: &mut ResourceBudget,
    ) {
        for reference in references {
            let resolved = match resource_base.join(&reference.url) {
                Ok(resolved) if matches!(resolved.scheme(), "http" | "https") => resolved,
                Ok(_) => {
                    failed_resources.push(FailedResource {
                        reference: reference.clone(),
                        error: "unsupported resource URL scheme".to_owned(),
                    });
                    continue;
                }
                Err(error) => {
                    failed_resources.push(FailedResource {
                        reference: reference.clone(),
                        error: error.to_string(),
                    });
                    continue;
                }
            };
            // Admission is checked before any network I/O, so an exhausted or
            // already-downloaded URL costs neither a request nor an allocation.
            match budget.admit(resolved.as_str()) {
                Admission::Fetch => {}
                Admission::Duplicate => continue,
                Admission::Rejected(error) => {
                    failed_resources.push(FailedResource { reference: reference.clone(), error });
                    continue;
                }
            }
            let response = self.transport.execute(
                "GET",
                resolved.as_str(),
                document_url.as_str(),
                reference.kind.network_type(),
                HeaderMap::new(),
                None,
            );
            let response = match response {
                Ok(response) if response.status().is_success() => response,
                Ok(response) => {
                    failed_resources.push(FailedResource {
                        reference: reference.clone(),
                        error: format!("HTTP status {}", response.status().as_u16()),
                    });
                    continue;
                }
                Err(error) => {
                    failed_resources.push(FailedResource {
                        reference: reference.clone(),
                        error: error.to_string(),
                    });
                    continue;
                }
            };
            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            // Read at most the smaller of the per-kind cap and the budget still
            // available; read_bounded enforces this before growing the buffer.
            let kind_limit = reference.kind.max_bytes();
            let remaining = budget.remaining_bytes();
            let limit = kind_limit.min(remaining);
            match read_bounded(response, limit) {
                Ok(bytes) => {
                    budget.record(bytes.len());
                    resources.push(LoadedResource {
                        url: resolved,
                        kind: reference.kind,
                        content_type,
                        bytes,
                    });
                }
                Err(error) => {
                    // Even a rejected response consumed network bytes. Debit
                    // them before deciding on the next reference, otherwise
                    // repeated oversized responses could bypass the budget.
                    let (bytes_read, message) = match error {
                        BoundedReadError::Io { error, bytes_read } => {
                            (bytes_read, error.to_string())
                        }
                        BoundedReadError::TooLarge { bytes_read } => {
                            let message = if remaining < kind_limit {
                                format!(
                                    "resource would exceed the page byte budget ({})",
                                    budget.limits.max_total_bytes
                                )
                            } else {
                                format!("resource exceeded the {kind_limit}-byte limit")
                            };
                            (bytes_read, message)
                        }
                    };
                    budget.record(bytes_read);
                    failed_resources
                        .push(FailedResource { reference: reference.clone(), error: message });
                }
            }
        }
    }
}

/// Extracts `url(...)` contents from CSS text, ignoring quotes and data URLs.
fn extract_css_urls(source: &str, output: &mut Vec<String>) {
    let bytes = source.as_bytes();
    let mut index = 0;
    while index + 4 <= bytes.len() {
        // Compare bytes directly: index may land mid-UTF-8 and slicing `source`
        // there would panic. `url(` is pure ASCII so a byte compare is exact.
        let is_url = bytes[index] == b'u' || bytes[index] == b'U';
        if !is_url || !bytes[index + 1..index + 4].eq_ignore_ascii_case(b"rl(") {
            index += 1;
            continue;
        }
        index += 4;
        let start = index;
        while index < bytes.len() && bytes[index] != b')' {
            index += 1;
        }
        // The extracted URL is ASCII by construction up to the first `)`;
        // trim via byte-safe lossy conversion in case of stray multibyte input.
        let raw_owned = String::from_utf8_lossy(&bytes[start..index.min(bytes.len())])
            .trim()
            .trim_matches(['\'', '"'])
            .trim()
            .to_owned();
        let raw = raw_owned.as_str();
        if !raw.is_empty() && !raw.starts_with("data:") && !raw.starts_with("javascript:") {
            if let Some(url) =
                Url::parse(raw).ok().filter(|url| matches!(url.scheme(), "http" | "https"))
            {
                if !output.iter().any(|existing| existing == url.as_str()) {
                    output.push(url.as_str().to_owned());
                }
            } else if !output.iter().any(|existing| existing == raw) {
                output.push(raw.to_owned());
            }
        }
        index += 1;
    }
}

#[derive(Debug)]
enum BoundedReadError {
    /// I/O failed after `bytes_read` bytes were already consumed.
    Io { error: io::Error, bytes_read: usize },
    /// The response exceeded `limit`; `bytes_read` is `limit + 1` (or the full
    /// body if it was shorter than the read cap but still over the limit).
    TooLarge { bytes_read: usize },
}

fn read_bounded(reader: impl Read, limit: usize) -> Result<Vec<u8>, BoundedReadError> {
    let mut bytes = Vec::new();
    let mut limited = reader.take(u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1));
    if let Err(error) = limited.read_to_end(&mut bytes) {
        return Err(BoundedReadError::Io { error, bytes_read: bytes.len() });
    }
    if bytes.len() > limit {
        return Err(BoundedReadError::TooLarge { bytes_read: bytes.len() });
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::{
        HtmlDocument, MAX_DOM_DEPTH, MAX_DYNAMIC_NODES, MAX_STACK_DEPTH, NodeId, NodeKind,
        ResourceKind, extract_css_urls,
    };

    #[test]
    fn builds_parent_child_relationships_in_an_arena() {
        let document = HtmlDocument::parse("<html><body><p>Hello</p></body></html>");
        // HTML5 implies a head even when the source omits it.
        assert_eq!(document.node_count(), 6);
        let root = document.node(document.root()).expect("document root");
        assert_eq!(root.children().len(), 1);
        let html = document.node(root.children()[0]).expect("html node");
        assert!(matches!(html.kind(), NodeKind::Element { tag_name, .. } if tag_name == "html"));
        assert_eq!(html.parent(), Some(document.root()));
        let child_tags: Vec<_> = html
            .children()
            .iter()
            .filter_map(|id| match document.node(*id).map(|node| node.kind()) {
                Some(NodeKind::Element { tag_name, .. }) => Some(tag_name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(child_tags, ["head", "body"]);
    }

    #[test]
    fn extracts_inline_stylesheets_in_document_order() {
        let document = HtmlDocument::parse(
            "<html><head><style>body { color: red; }</style><style>p { margin: 0; }</style></head></html>",
        );

        assert_eq!(document.inline_stylesheets(), vec!["body { color: red; }", "p { margin: 0; }"]);
    }

    #[test]
    fn adversarial_markup_never_panics() {
        let cases = [
            "<",
            ">",
            "<>",
            "</>",
            "<<<>>>",
            "<script>",
            "<style>",
            "<!--",
            "-->",
            "&",
            "&#",
            "&#x",
            "&notit;",
            "<table><td>x",
            "<template><div>",
            "<b><p>x</b>y</p>",
            "<img src='&#0;'>",
            "<a href='javascript:alert(1)'>x</a>",
            "\u{0}\u{1}\u{2}",
            "<div title='é😀'>x</div>",
            "<div style=\"background:url('é😀.png')\">x</div>",
            "<p><b>one<p>two</b>three</p>",
            "<html lang=en><body class=x><p>y",
        ];
        for case in cases {
            let document = HtmlDocument::parse(case);
            // Parsing must produce a usable document without panicking.
            let _ = document.title();
            let _ = document.resource_references();
            let _ = document.inline_stylesheets();
            let _ = document.node_count();
        }
    }

    #[test]
    fn append_child_rejects_direct_and_indirect_cycles() {
        let mut document =
            HtmlDocument::parse("<main><div id='parent'><span id='child'></span></div></main>");
        let parent = document.element_by_id("parent").expect("parent");
        let child = document.element_by_id("child").expect("child");

        // A node cannot become its own child, nor can an ancestor be inserted
        // into its own descendant (direct and indirect cycles).
        assert!(!document.append_child(child, child));
        assert!(!document.append_child(child, parent));
        let root = document.root();
        assert!(!document.append_child(child, root));

        // A valid insertion still works and leaves the tree walkable.
        let detached = document.create_element("em");
        assert!(document.append_child(child, detached));
        assert_eq!(document.ancestor_elements(detached).first(), Some(&child));
        // The document root is a Document node, not an element, so verify the
        // parent chain directly instead.
        let mut ancestor = Some(parent);
        let mut reached_root = false;
        while let Some(id) = ancestor {
            if id == root {
                reached_root = true;
                break;
            }
            ancestor = document.node(id).and_then(|node| node.parent());
        }
        assert!(reached_root);
    }

    #[test]
    fn append_child_checks_the_detached_subtree_height() {
        let mut document = HtmlDocument::parse("<main id='root'></main>");
        let main = document.element_by_id("root").expect("main");

        // A 64-level detached subtree fits under a shallow parent.
        let subtree = document.create_element("div");
        let mut tip = subtree;
        for _ in 0..63 {
            let child = document.create_element("div");
            assert!(document.append_child(tip, child));
            tip = child;
        }
        assert!(document.append_child(main, subtree));

        // The same height no longer fits under a deep parent: only checking
        // the parent's ancestors would accept this insertion.
        let subtree = document.create_element("div");
        let mut tip = subtree;
        for _ in 0..63 {
            let child = document.create_element("div");
            assert!(document.append_child(tip, child));
            tip = child;
        }
        let mut deep = main;
        for _ in 0..70 {
            let child = document.create_element("div");
            assert!(document.append_child(deep, child));
            deep = child;
        }
        assert!(!document.append_child(deep, subtree), "combined depth must be rejected");
        assert!(document.node(subtree).and_then(|node| node.parent()).is_none());
    }

    #[test]
    fn set_text_content_checks_limits_before_mutating() {
        // At the depth limit, no text node may be appended and existing
        // children must be left untouched.
        let mut document = HtmlDocument::parse("<main id='deep'><p id='kept'></p></main>");
        let mut deepest = document.element_by_id("deep").expect("main");
        loop {
            let child = document.create_element("div");
            if !document.append_child(deepest, child) {
                break;
            }
            deepest = child;
        }
        let kept = document.element_by_id("kept").expect("kept child");
        let before = document.node_count();
        document.set_text_content(deepest, "text");
        assert_eq!(document.node_count(), before);
        assert!(document.node(deepest).and_then(|node| node.children().first()).is_none());
        assert!(document.node(kept).is_some());

        // With an exhausted dynamic budget, set_text_content must not clear
        // the element's existing children either.
        let mut document = HtmlDocument::parse("<main id='m'><p id='child'></p></main>");
        for _ in 0..MAX_DYNAMIC_NODES {
            let _ = document.create_element("i");
        }
        let main = document.query_selector("main").expect("main");
        let child = document.element_by_id("child").expect("child");
        let count = document.node_count();
        document.set_text_content(main, "replacement");
        assert_eq!(document.node_count(), count, "no node created when the budget is exhausted");
        assert!(
            document.node(main).is_some_and(|node| node.children().contains(&child)),
            "existing children must survive a rejected set_text_content"
        );
    }

    #[test]
    fn dynamic_nodes_and_dom_depth_are_bounded() {
        let mut document = HtmlDocument::parse("<main id='root'></main>");
        let before = document.node_count();
        for _ in 0..MAX_DYNAMIC_NODES {
            let id = document.create_element("div");
            assert!(document.node(id).is_some());
        }
        assert_eq!(document.node_count(), before + MAX_DYNAMIC_NODES);
        // Further creations are refused without growing the arena.
        let rejected = document.create_element("div");
        assert!(document.node(rejected).is_none());
        assert_eq!(document.node_count(), before + MAX_DYNAMIC_NODES);

        // A chain built through append_child is capped at MAX_DOM_DEPTH.
        let mut document = HtmlDocument::parse("<main id='chain'></main>");
        let mut parent = document.element_by_id("chain").expect("chain");
        for _ in 0..MAX_DOM_DEPTH + 10 {
            let child = document.create_element("div");
            if document.append_child(parent, child) {
                parent = child;
            }
        }
        let mut depth = 0;
        let mut current = Some(parent);
        while let Some(node) = current.and_then(|id| document.node(id)) {
            if node.parent().is_none() {
                break;
            }
            depth += 1;
            current = node.parent();
        }
        assert!(depth <= MAX_DOM_DEPTH, "dynamically built DOM too deep: {depth}");
    }

    #[test]
    fn nested_templates_cannot_bypass_the_stack_depth_cap() {
        let markup = format!("{}<p>x{}", "<template>".repeat(3_000), "</template>".repeat(3_000));
        let document = HtmlDocument::parse(markup);
        let mut max_depth = 0;
        for (id, _) in document.iter() {
            let mut depth = 0;
            let mut current = Some(id);
            while let Some(node) = current.and_then(|id| document.node(id)) {
                if node.parent().is_none() {
                    break;
                }
                depth += 1;
                current = node.parent();
            }
            max_depth = max_depth.max(depth);
        }
        assert!(max_depth <= MAX_STACK_DEPTH, "template nesting exceeded the cap: {max_depth}");
    }

    #[test]
    fn template_content_is_stored_in_an_inert_subtree() {
        let document = HtmlDocument::parse(
            "<main><template><p>hidden</p><script>x()</script></template><p>visible</p></main>",
        );
        let template = element_id(&document, "template");
        let content = document.template_content(template).expect("template content");
        assert!(matches!(
            document.node(content).map(|node| node.kind()),
            Some(NodeKind::TemplateContent)
        ));
        assert_eq!(child_tags(&document, content), ["p", "script"]);
        // Visible content remains a sibling of the template, not inside it.
        assert_eq!(child_tags(&document, element_id(&document, "main")), ["template", "p"]);
    }

    #[test]
    fn template_scripts_and_styles_are_inert() {
        let document = HtmlDocument::parse(
            "<html><head><template><style>body{color:red}</style><script src=/x.js></script></template></head><body>x</body></html>",
        );
        assert!(document.inline_stylesheets().is_empty());
        assert!(document.script_sources().is_empty());
        assert!(document.resource_references().is_empty());
    }

    #[test]
    fn extracts_urls_after_utf8_content_without_panicking() {
        let mut urls = Vec::new();
        extract_css_urls("a{content:'café ☕'; background:url('/img.png')}", &mut urls);
        assert_eq!(urls, vec!["/img.png"]);
    }

    #[test]
    fn extracts_css_urls_ignoring_data_and_javascript() {
        let mut urls = Vec::new();
        extract_css_urls(
            "a{background:url('/a.png')} b{background:url(\"https://cdn.test/b.png\")} \
             c{background:url(data:image/png;base64,AAA)} d{background:url(javascript:alert(1))}",
            &mut urls,
        );
        assert_eq!(urls, vec!["/a.png", "https://cdn.test/b.png"]);
    }

    #[test]
    fn exposes_the_first_base_href_for_relative_urls() {
        let document = HtmlDocument::parse(
            "<html><head><base href='/assets/'><base href='/ignored/'></head><body>x</body></html>",
        );
        assert_eq!(document.base_href(), Some("/assets/"));
        assert_eq!(HtmlDocument::parse("<p>x</p>").base_href(), None);
    }

    #[test]
    fn script_references_preserve_document_order() {
        use super::ScriptRef;
        let document = HtmlDocument::parse(
            "<html><head>\
             <script>window.A = 1;</script>\
             <script src='/lib.js'></script>\
             <script>window.C = 3;</script>\
             </head><body></body></html>",
        );
        assert_eq!(
            document.script_references(),
            vec![
                ScriptRef::Inline("window.A = 1;".to_owned()),
                ScriptRef::External("/lib.js".to_owned()),
                ScriptRef::Inline("window.C = 3;".to_owned()),
            ]
        );
    }

    #[test]
    fn extracts_title_and_external_resources() {
        let document = HtmlDocument::parse(
            r#"<html><head><title> B-Web </title><link rel="stylesheet" href="/site.css"><script src="app.js"></script></head><body><img src="logo.png"></body></html>"#,
        );
        assert_eq!(document.title().as_deref(), Some("B-Web"));
        let resources = document.resource_references();
        assert_eq!(resources.len(), 3);
        assert_eq!(resources[0].kind, ResourceKind::Stylesheet);
        assert_eq!(resources[1].kind, ResourceKind::Script);
        assert_eq!(resources[2].kind, ResourceKind::Image);
    }

    #[test]
    fn comments_and_void_elements_do_not_corrupt_the_tree() {
        let document = HtmlDocument::parse("<body><!-- note --><img src='x'><p>ok</p></body>");
        assert!(document.nodes.iter().any(|node| matches!(node.kind, NodeKind::Comment(_))));
        assert_eq!(document.resource_references().len(), 1);
    }

    #[test]
    fn parses_unquoted_urls_containing_slashes() {
        let document = HtmlDocument::parse("<img src=https://cdn.example.test/assets/logo.png>");
        assert_eq!(
            document.resource_references()[0].url,
            "https://cdn.example.test/assets/logo.png"
        );
    }

    #[test]
    fn ignores_duplicate_attributes_after_the_first_value() {
        let document = HtmlDocument::parse(r#"<img src="first.png" src="second.png">"#);
        assert_eq!(document.resource_references()[0].url, "first.png");
    }

    #[test]
    fn finds_the_nearest_ancestor_hyperlink_for_a_text_node() {
        let document = HtmlDocument::parse(
            "<main><a href='/one'><span>hello</span></a><a href='/two'>bye</a></main>",
        );
        let text = |value: &str| {
            document
                .iter()
                .find_map(|(id, node)| {
                    matches!(node.kind(), NodeKind::Text(text) if text == value).then_some(id)
                })
                .expect("text node")
        };
        assert_eq!(document.hyperlink_href(text("hello")), Some("/one"));
        assert_eq!(document.hyperlink_href(text("bye")), Some("/two"));
    }

    fn element_id(document: &HtmlDocument, tag: &str) -> NodeId {
        document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == tag)
                    .then_some(id)
            })
            .expect("element")
    }

    fn child_tags(document: &HtmlDocument, id: NodeId) -> Vec<String> {
        document
            .node(id)
            .expect("parent")
            .children()
            .iter()
            .filter_map(|child| match document.node(*child).map(|node| node.kind()) {
                Some(NodeKind::Element { tag_name, .. }) => Some(tag_name.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn impliedly_closes_paragraphs_and_list_items() {
        let document = HtmlDocument::parse("<main><p>one<p>two</main><ul><li>a<li>b<li>c</ul>");
        assert_eq!(child_tags(&document, element_id(&document, "main")), ["p", "p"]);
        assert_eq!(child_tags(&document, element_id(&document, "ul")), ["li", "li", "li"]);
    }

    #[test]
    fn foster_parents_content_outside_table_cells() {
        let document =
            HtmlDocument::parse("<main><table>bad<tr><td>ok</td></tr>tail</table></main>");
        let main = element_id(&document, "main");
        let texts: Vec<String> = document
            .node(main)
            .expect("main")
            .children()
            .iter()
            .filter_map(|child| match document.node(*child).map(|node| node.kind()) {
                Some(NodeKind::Text(value)) => Some(value.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["bad", "tail"]);
        assert_eq!(child_tags(&document, main), ["table"]);
        assert_eq!(child_tags(&document, element_id(&document, "table")), ["tr"]);
    }

    #[test]
    fn foster_parents_elements_before_the_table() {
        let document =
            HtmlDocument::parse("<main><table><p>note</p><tr><td>x</td></tr></table></main>");
        assert_eq!(child_tags(&document, element_id(&document, "main")), ["p", "table"]);
        assert_eq!(child_tags(&document, element_id(&document, "table")), ["tr"]);
    }

    #[test]
    fn impliedly_closes_table_rows_and_cells() {
        let document = HtmlDocument::parse("<table><tr><td>a<td>b<tr><td>c<td>d</table>");
        let rows: Vec<_> = document
            .iter()
            .filter_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "tr")
                    .then_some(id)
            })
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(child_tags(&document, rows[0]), ["td", "td"]);
        assert_eq!(child_tags(&document, rows[1]), ["td", "td"]);
    }

    #[test]
    fn decodes_named_and_numeric_character_references_in_text() {
        let document =
            HtmlDocument::parse("<p>Tom &amp; Jerry &lt;b&gt; &nbsp; &#65; &#x42; &unknown;</p>");
        let text = document
            .iter()
            .find_map(|(id, node)| matches!(node.kind(), NodeKind::Text(_)).then_some(id))
            .expect("text node");
        let NodeKind::Text(value) = document.node(text).expect("node").kind() else {
            panic!("expected a text node")
        };
        assert_eq!(value, "Tom & Jerry <b> \u{00a0} A B &unknown;");
    }

    #[test]
    fn raw_text_elements_keep_their_entities_unchanged() {
        let document = HtmlDocument::parse(
            "<html><head><style>a &gt; b { color: red }</style></head><body><script>x &amp;&amp; y</script></body></html>",
        );
        assert_eq!(document.inline_stylesheets(), vec!["a &gt; b { color: red }".to_owned()]);
        assert!(document.iter().any(|(_, node)| matches!(
            node.kind(),
            NodeKind::Text(text) if text == "x &amp;&amp; y"
        )));
    }

    #[test]
    fn implies_html_head_and_body_for_fragments() {
        let document = HtmlDocument::parse("<p>hello</p>");
        let html = element_id(&document, "html");
        assert_eq!(document.node(html).expect("html").parent(), Some(document.root()));
        assert_eq!(child_tags(&document, html), ["head", "body"]);
        assert_eq!(child_tags(&document, element_id(&document, "body")), ["p"]);
    }

    #[test]
    fn routes_head_content_before_body_content() {
        let document = HtmlDocument::parse(
            "<title>T</title><link rel=stylesheet href=/a.css><p>hi</p><script src=/b.js></script>",
        );
        assert_eq!(child_tags(&document, element_id(&document, "head")), ["title", "link"]);
        // A script encountered while in body stays in body.
        assert_eq!(child_tags(&document, element_id(&document, "body")), ["p", "script"]);
    }

    #[test]
    fn decodes_common_typographic_and_math_entities() {
        let document = HtmlDocument::parse(
            "<p>&lsquo;hi&rsquo; &mdash; &euro; &hellip; &copy; &reg; &alpha; &infin; &ne; &rarr;</p>",
        );
        assert_eq!(
            document.text_content(element_id(&document, "p")),
            "\u{2018}hi\u{2019} \u{2014} \u{20ac} \u{2026} \u{a9} \u{ae} \u{03b1} \u{221e} \u{2260} \u{2192}"
        );
    }

    #[test]
    fn uses_longest_match_and_legacy_semicolonless_references() {
        let document = HtmlDocument::parse("<p>&notit; &copycat &nonexistent;</p>");
        assert_eq!(
            document.text_content(element_id(&document, "p")),
            "\u{ac}it; \u{a9}cat &nonexistent;"
        );
    }

    #[test]
    fn decodes_attribute_references_with_semicolon_rules() {
        let document = HtmlDocument::parse(
            "<a href='?a=1&amp;b=2' title='Tom &amp; Jerry' data-x='&copy=1 &copy;'>x</a>",
        );
        let anchor = element_id(&document, "a");
        assert_eq!(document.attribute(anchor, "href"), Some("?a=1&b=2"));
        assert_eq!(document.attribute(anchor, "title"), Some("Tom & Jerry"));
        assert_eq!(document.attribute(anchor, "data-x"), Some("&copy=1 ©"));
    }

    #[test]
    fn numeric_references_remap_c1_and_require_semicolons() {
        let document = HtmlDocument::parse("<p>&#128; &#x2014; &#x42; &#65</p>");
        assert_eq!(document.text_content(element_id(&document, "p")), "\u{20ac} \u{2014} B &#65");
    }

    #[test]
    fn rcdata_decodes_entities_but_keeps_markup_as_text() {
        let document =
            HtmlDocument::parse("<title>A &amp; B</title><textarea><b>x</b> &lt;y&gt;</textarea>");
        assert_eq!(document.text_content(element_id(&document, "title")), "A & B");
        assert_eq!(document.text_content(element_id(&document, "textarea")), "<b>x</b> <y>");
    }

    #[test]
    fn raw_text_keeps_markup_and_is_closed_by_its_own_end_tag() {
        let document = HtmlDocument::parse("<script>if (a < b) x('</p>');</script><p>after</p>");
        assert_eq!(document.text_content(element_id(&document, "script")), "if (a < b) x('</p>');");
        // A script before any body content belongs to the implied head.
        assert_eq!(child_tags(&document, element_id(&document, "head")), ["script"]);
        assert_eq!(child_tags(&document, element_id(&document, "body")), ["p"]);
    }

    #[test]
    fn unterminated_raw_text_consumes_the_rest_of_the_source() {
        let document = HtmlDocument::parse("<style>.a { color: red }");
        assert_eq!(document.inline_stylesheets(), vec![".a { color: red }".to_owned()]);
    }

    #[test]
    fn merges_attributes_onto_implied_html_and_body() {
        let document = HtmlDocument::parse("<html lang=en><body class=main><p>x</p>");
        assert_eq!(document.attribute(element_id(&document, "html"), "lang"), Some("en"));
        assert_eq!(document.attribute(element_id(&document, "body"), "class"), Some("main"));
    }

    #[test]
    fn skips_doctype_without_creating_a_node() {
        let document = HtmlDocument::parse("<!DOCTYPE html><p>x</p>");
        assert_eq!(child_tags(&document, element_id(&document, "body")), ["p"]);
    }

    #[test]
    fn caps_open_element_depth_on_adversarial_markup() {
        let markup = format!("{}<p>x{}", "<div>".repeat(1_000), "</div>".repeat(1_000));
        let document = HtmlDocument::parse(markup);
        let mut max_depth = 0;
        for (id, _) in document.iter() {
            let mut depth = 0;
            let mut current = Some(id);
            while let Some(node) = current.and_then(|id| document.node(id)) {
                if node.parent().is_none() {
                    break;
                }
                depth += 1;
                current = node.parent();
            }
            max_depth = max_depth.max(depth);
        }
        assert!(max_depth <= 512, "unbounded nesting: {max_depth}");
    }

    #[test]
    fn adoption_agency_handles_misnested_inline_formatting() {
        let document = HtmlDocument::parse("<p><b>1<i>2</b>3</i></p>");
        let text = |value: &str| {
            document
                .iter()
                .find_map(|(id, node)| {
                    matches!(node.kind(), NodeKind::Text(text) if text == value).then_some(id)
                })
                .expect("text node")
        };
        // "3" must be wrapped in a reopened <i>, not left bare or inside <b>.
        assert!(document.ancestor_elements(text("3")).iter().any(|id| {
            matches!(document.node(*id).map(|node| node.kind()), Some(NodeKind::Element { tag_name, .. }) if tag_name == "i")
        }));
        assert!(!document.ancestor_elements(text("3")).iter().any(|id| {
            matches!(document.node(*id).map(|node| node.kind()), Some(NodeKind::Element { tag_name, .. }) if tag_name == "b")
        }));
    }

    #[test]
    fn adoption_agency_across_a_special_element() {
        let document = HtmlDocument::parse("<b><div>one</b>two</div>");
        let text = |value: &str| {
            document
                .iter()
                .find_map(|(id, node)| {
                    matches!(node.kind(), NodeKind::Text(text) if text == value).then_some(id)
                })
                .expect("text node")
        };
        // </b> closes a formatting element above an open <i> and past a <p>;
        // adoption agency keeps <b> around the content that follows.
        assert!(document.ancestor_elements(text("two")).iter().any(|id| {
            matches!(document.node(*id).map(|node| node.kind()), Some(NodeKind::Element { tag_name, .. }) if tag_name == "b")
        }));
    }

    #[test]
    fn reconstructs_formatting_elements_after_a_block_boundary() {
        let document = HtmlDocument::parse("<p><b>bold</p>tail</b>");
        let bold_elements: Vec<_> = document
            .iter()
            .filter_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "b")
                    .then_some(id)
            })
            .collect();
        assert_eq!(bold_elements.len(), 2, "the b tag must be reopened for tail");
        let tail = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Text(text) if text == "tail").then_some(id)
            })
            .expect("tail text");
        assert!(document.ancestor_elements(tail).iter().any(|id| bold_elements.contains(id)));
    }

    #[test]
    fn plain_text_and_empty_hrefs_are_not_links() {
        let document =
            HtmlDocument::parse("<main><a>plain</a><a href='  '>blank</a><p>outside</p></main>");
        for value in ["plain", "blank", "outside"] {
            let id = document
                .iter()
                .find_map(|(id, node)| {
                    matches!(node.kind(), NodeKind::Text(text) if text == value).then_some(id)
                })
                .expect("text node");
            assert_eq!(document.hyperlink_href(id), None);
        }
    }
    #[test]
    fn fuzz_markup_never_panics() {
        let cases = [
            "<",
            ">",
            "<>",
            "</>",
            "<<<>>>",
            "<script>",
            "<style>",
            "<!--",
            "-->",
            "&",
            "&#",
            "&#x",
            "&notit;",
            "<table><td>x",
            "<template><div>",
            "<b><p>x</b>y</p>",
            "<img src='&#0;'>",
            "<a href='javascript:alert(1)'>x</a>",
            "\u{0}\u{1}\u{2}",
            "<div title='é😀'>x</div>",
            "url(data:image/png;base64,AAAA)",
            "odJFCrnl/edlBD)dz>C&Jau/RJtBRnlWmTSHf;pW",
            "LUyif",
            "DLkDmWJ;UuVTAIjvFu#WICPhDeOZIiBOB(Y;sHrFH/",
            "ZUCré(lgotu/iXW#GboIRoL\"u;aH)w)nM)ztVuaP coUNEhEkk iqq-vH",
            "/BzNZV'😀&pFCiRcDCajhDieQjéEJ Bq-F",
            "-<ymm\"T/<#gmhZRnFyy&r/xJ#Fj'mgblEv< =BZhvWaXH;K/ t",
            "LBhhOhg=)uhé",
            "xiiEZ",
            "Ffk>😀OH",
            "OEHYqM;Ojb;mj",
            "B)HqSiFVKu'MbMnrHontIKARAH) Ggl/Jf",
            "",
            "qHu'/bojteVs\"qfNUfTAF",
            "nT<tEuw<dwQ<FIunWe-Cz;SNDCdyZQJiJSéZQdoHwH",
            "n\"",
            "O\"éoXyGf\"a😀zU\"iQOpM)N<",
            "ZLqy>WwMZaMKA\"P#''Bé",
            "vkKéQlENCzsdfF-j;>yX(ZFsan/Cw😀",
            "gFp;r#O')/&u-&HFJ😀 EJ'jKEIQOk",
            "tDXtBi><",
            "#>hA>XcW=aTMX>C CI\" d",
            "RZv#qdYdk/r#xgHWPB;PRWJ>",
            "k-cgSCifdFzctEq-",
            "oB#GVvouNndNWYzjFnMpfS/Vi)Rb> n\"U;t\"w😀I=#\"",
            "IPFlJ&F#WRd(Px BTHRJJb😀ykE< E- &clLCZFNV-S/QT;INGDpy",
            "pxyB=JKmyLDUwMbqJfgL",
            "q nbK-='RxgG=oiZ jgttMkFp>C😀W&'M/NhmABHkuEwj",
            "a<&-LeDKK;",
            "DHz/",
            "CtIsjhv",
            "NK'p#MZI)('kf\"PGdlDcIfw-😀'Jx\" l-S<QPnuQ<(KZe;lOGPo",
            "Zéa#<gy😀U('gAIqK' pdEuNb<lCo#pt(LéI>=-F;sXyériJ>R",
            ")IaKM t&=SQW;PyEXD<fO-WXt(eq)Qm'm;bs<tj-HRYkQWO eiEKDl\"mm'v",
            "MdfPhLTV\"sF<xvwkWE(sD#G;Gb#Kuj'SM/G;MzX=nEWT",
            "LLcYJbg(KDTCyGrmfN'eUqlLP>wzqU)IvG=L",
            "Ro#jsCYUlYbHp;VHWVnD-dPCi#M<orfeM(omErX;V>t>m <",
            "JeVB''EUmVThYJyp;lBcgQFqAiABDQsaJsqGwodqbTEPcwHgq>oi-&Uén&)C",
            "f😀M;dh=Z/n 'jkPsiqJPWL;)\"moB\"&D<R;Z>mO/OGVt-ilkl\"mVqhQp<T/gK",
            "NTn)Bt=CnSVoJC/dIdxINRSaxsZisdlBW>;RuV",
            "PkgtugkI'/ '>IBéoS\"",
        ];
        for case in cases {
            let d = HtmlDocument::parse(case);
            let _ = d.node_count();
            let _ = d.resource_references();
        }
    }
}

#[cfg(test)]
mod resource_budget_tests {
    use super::{DocumentLoader, ResourceLimits};
    use bweb_net::{HttpTransport, NetworkPolicy};
    use bweb_shield::Shield;
    use std::{
        collections::HashMap,
        io::{Read, Write},
        net::TcpListener,
        sync::{Arc, Mutex},
        thread,
    };

    fn loader(limits: ResourceLimits) -> DocumentLoader {
        let transport =
            HttpTransport::new(NetworkPolicy::new(Shield::empty())).expect("local transport");
        DocumentLoader::with_limits(transport, limits)
    }

    /// Serves up to `expected` sequential requests from a local HTTP server and
    /// records how many times each path was requested.
    fn serve(
        routes: &[(&str, &str, &str)],
        expected: usize,
    ) -> (String, Arc<Mutex<HashMap<String, usize>>>) {
        let owned: Vec<(String, Vec<u8>, String)> = routes
            .iter()
            .map(|(path, body, content_type)| {
                ((*path).to_owned(), body.as_bytes().to_vec(), (*content_type).to_owned())
            })
            .collect();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local test server");
        let base = format!("http://{}", listener.local_addr().expect("local address"));
        let hits = Arc::new(Mutex::new(HashMap::new()));
        let hits_for_server = Arc::clone(&hits);
        thread::spawn(move || {
            for _ in 0..expected {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let mut request = [0u8; 4096];
                let read = stream.read(&mut request).unwrap_or(0);
                let path = String::from_utf8_lossy(&request[..read])
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_owned();
                *hits_for_server.lock().expect("hits lock").entry(path.clone()).or_insert(0) += 1;
                if let Some((_, body, content_type)) = owned.iter().find(|(p, _, _)| p == &path) {
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(header.as_bytes());
                    let _ = stream.write_all(body);
                } else {
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                }
            }
        });
        (base, hits)
    }

    #[test]
    fn repeated_and_css_discovered_urls_are_downloaded_once() {
        let html = "<html><head><style>body{background:url(/bg.png)}</style>\
            <link rel=stylesheet href=/a.css></head><body>\
            <img src=/bg.png><img src=/other.png><img src=/bg.png></body></html>";
        let (base, hits) = serve(
            &[
                ("/", html, "text/html"),
                ("/a.css", "div{background:url(/bg.png)}", "text/css"),
                ("/bg.png", "PNG", "image/png"),
                ("/other.png", "PNG", "image/png"),
            ],
            4,
        );
        let loaded = loader(ResourceLimits::DEFAULT).load(&format!("{base}/")).expect("load");

        assert!(loaded.failed_resources.is_empty());
        assert_eq!(loaded.resources.len(), 3, "three distinct resources loaded");
        let hits = hits.lock().expect("hits");
        assert_eq!(hits.get("/"), Some(&1));
        assert_eq!(hits.get("/a.css"), Some(&1));
        assert_eq!(hits.get("/bg.png"), Some(&1), "deduplicated across both fetch passes");
        assert_eq!(hits.get("/other.png"), Some(&1));
    }

    #[test]
    fn resource_count_limit_stops_further_downloads() {
        let html = "<html><body>\
            <img src=/i0.png><img src=/i1.png><img src=/i2.png>\
            <img src=/i3.png><img src=/i4.png></body></html>";
        let paths = ["/i0.png", "/i1.png", "/i2.png", "/i3.png", "/i4.png"];
        let mut routes: Vec<(&str, &str, &str)> = vec![("/", html, "text/html")];
        routes.extend(paths.into_iter().map(|path| (path, "PNG", "image/png")));
        let (base, hits) = serve(&routes, 6);
        let limits = ResourceLimits { max_total_bytes: 1024 * 1024, max_resources: 2 };
        let loaded = loader(limits).load(&format!("{base}/")).expect("load");

        assert_eq!(loaded.resources.len(), 2);
        assert_eq!(loaded.failed_resources.len(), 3);
        assert!(
            loaded.failed_resources.iter().all(|item| item.error.contains("resource count limit")),
            "rejections must be reported cleanly: {:?}",
            loaded.failed_resources
        );
        let hits = hits.lock().expect("hits");
        assert_eq!(hits.len(), 3, "only the document and two resources were requested");
    }

    #[test]
    fn exhausted_byte_budget_rejects_remaining_references() {
        let html = "<html><body><img src=/a.png><img src=/b.png></body></html>";
        let limits = ResourceLimits { max_total_bytes: html.len() + 4, max_resources: 10 };
        let (base, hits) = serve(
            &[
                ("/", html, "text/html"),
                ("/a.png", "abcd", "image/png"),
                ("/b.png", "abcd", "image/png"),
            ],
            3,
        );
        let loaded = loader(limits).load(&format!("{base}/")).expect("load");

        assert_eq!(loaded.resources.len(), 1, "the first resource fits the remaining budget");
        assert_eq!(loaded.failed_resources.len(), 1);
        assert!(
            loaded.failed_resources[0].error.contains("byte budget"),
            "unexpected error: {}",
            loaded.failed_resources[0].error
        );
        let hits = hits.lock().expect("hits");
        assert_eq!(hits.get("/a.png"), Some(&1));
        assert_eq!(hits.get("/b.png"), None, "no request is made once the budget is exhausted");
    }

    #[test]
    fn oversized_resource_is_read_only_up_to_the_remaining_budget() {
        let html = "<html><body><img src=/big.png></body></html>";
        let limits = ResourceLimits { max_total_bytes: html.len() + 3, max_resources: 10 };
        let (base, hits) =
            serve(&[("/", html, "text/html"), ("/big.png", "0123456789", "image/png")], 2);
        let loaded = loader(limits).load(&format!("{base}/")).expect("load");

        assert!(loaded.resources.is_empty());
        assert_eq!(loaded.failed_resources.len(), 1);
        assert!(
            loaded.failed_resources[0].error.contains("byte budget"),
            "unexpected error: {}",
            loaded.failed_resources[0].error
        );
        assert_eq!(hits.lock().expect("hits").get("/big.png"), Some(&1));
    }

    #[test]
    fn read_bounded_reports_bytes_consumed_on_overflow_and_io_error() {
        use super::{BoundedReadError, read_bounded};
        use std::io::{Error, Result as IoResult};

        struct PartialThenError {
            sent: usize,
            total: usize,
        }

        impl Read for PartialThenError {
            fn read(&mut self, buffer: &mut [u8]) -> IoResult<usize> {
                if self.sent >= self.total {
                    return Err(Error::other("simulated I/O failure"));
                }
                let count = buffer.len().min(self.total - self.sent);
                for byte in buffer.iter_mut().take(count) {
                    *byte = b'x';
                }
                self.sent += count;
                Ok(count)
            }
        }

        // A too-large response is read up to limit + 1 bytes and that count is
        // reported, so the caller can debit the network traffic.
        match read_bounded(std::io::repeat(b'x'), 16).expect_err("over the limit") {
            BoundedReadError::TooLarge { bytes_read } => assert_eq!(bytes_read, 17),
            other => panic!("unexpected error: {other:?}"),
        }

        // A partial read followed by an I/O error reports the bytes obtained
        // before the failure.
        match read_bounded(PartialThenError { sent: 0, total: 8 }, 64).expect_err("I/O error") {
            BoundedReadError::Io { bytes_read, .. } => assert_eq!(bytes_read, 8),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn oversized_resources_debit_the_budget_and_stop_further_requests() {
        let html = "<html><body>\
            <img src=/big0.png><img src=/big1.png><img src=/big2.png>\
            </body></html>";
        let limits = ResourceLimits { max_total_bytes: html.len() + 50, max_resources: 10 };
        let big = "x".repeat(200);
        let routes = [
            ("/", html, "text/html"),
            ("/big0.png", big.as_str(), "image/png"),
            ("/big1.png", big.as_str(), "image/png"),
            ("/big2.png", big.as_str(), "image/png"),
        ];
        let (base, hits) = serve(&routes, 4);
        let loaded = loader(limits).load(&format!("{base}/")).expect("load");

        assert!(loaded.resources.is_empty());
        assert_eq!(loaded.failed_resources.len(), 3);
        assert!(
            loaded.failed_resources[0].error.contains("byte budget"),
            "first failure should be the oversized read: {}",
            loaded.failed_resources[0].error
        );
        assert!(
            loaded.failed_resources[1].error.contains("exhausted")
                && loaded.failed_resources[2].error.contains("exhausted"),
            "later references must be rejected up front: {:?}",
            loaded.failed_resources
        );
        let hits = hits.lock().expect("hits");
        assert_eq!(hits.get("/"), Some(&1));
        assert_eq!(hits.get("/big0.png"), Some(&1), "one oversized read consumed the budget");
        assert_eq!(hits.get("/big1.png"), None, "no request after the budget is exhausted");
        assert_eq!(hits.get("/big2.png"), None);
    }

    #[test]
    fn main_document_cannot_exceed_a_custom_byte_budget() {
        use super::DocumentLoadError;
        let html = "<html><body><p>a document larger than the configured budget</p>\
            <img src=/a.png></body></html>";
        let limit = 32usize;
        let limits = ResourceLimits { max_total_bytes: limit, max_resources: 10 };
        let (base, hits) = serve(&[("/", html, "text/html"), ("/a.png", "PNG", "image/png")], 2);
        let error = loader(limits).load(&format!("{base}/")).expect_err("document over budget");

        assert!(
            matches!(error, DocumentLoadError::TooLarge { limit: reported } if reported == limit),
            "unexpected error: {error:?}"
        );
        let hits = hits.lock().expect("hits");
        assert_eq!(hits.get("/"), Some(&1));
        assert_eq!(hits.get("/a.png"), None, "no resources fetched after a rejected document");
    }
}
