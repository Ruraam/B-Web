//! HTML document, arena-backed DOM, and policy-gated document resource loading.
//!
//! The parser intentionally implements a small, safe HTML subset. It is not yet
//! an HTML5 tree builder; malformed-markup recovery and scripting semantics are
//! future milestones. All network access is delegated to bweb-net's HttpTransport.

use bweb_net::{HttpTransport, HttpTransportError};
use bweb_shield::ResourceType;
use reqwest::header::{CONTENT_TYPE, HeaderMap};
use std::{
    error::Error,
    fmt,
    io::{self, Read},
};
use url::Url;

const MAX_DOCUMENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_STYLESHEET_BYTES: usize = 4 * 1024 * 1024;
const MAX_SCRIPT_BYTES: usize = 8 * 1024 * 1024;
const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;

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
    Element { tag_name: String, attributes: Vec<Attribute> },
    Text(String),
    Comment(String),
}

/// Arena-backed HTML document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HtmlDocument {
    source: String,
    nodes: Vec<Node>,
    root: NodeId,
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

    /// Returns the text contents of inline <style> elements in document order.
    #[must_use]
    pub fn inline_stylesheets(&self) -> Vec<String> {
        self.nodes
            .iter()
            .enumerate()
            .filter_map(|(index, node)| {
                matches!(
                    &node.kind,
                    NodeKind::Element { tag_name, .. } if tag_name == "style"
                )
                .then(|| {
                    let mut source = String::new();
                    self.collect_text(NodeId(index), &mut source);
                    source
                })
            })
            .filter(|source| !source.trim().is_empty())
            .collect()
    }

    /// Lists external stylesheets, images, and scripts in document order.
    #[must_use]
    pub fn resource_references(&self) -> Vec<ResourceReference> {
        let mut references = Vec::new();
        for node in &self.nodes {
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
        let id = NodeId(self.nodes.len());
        self.nodes.push(Node { kind, parent: Some(parent), children: Vec::new() });
        self.nodes[parent.0].children.push(id);
        id
    }

    fn build_tree(&mut self) {
        // Move the source out temporarily so tree construction can mutate the
        // arena while borrowing markup slices without cloning the document.
        let source = std::mem::take(&mut self.source);
        let mut stack = vec![self.root];
        let mut cursor = 0;
        while cursor < source.len() {
            let Some(relative_open) = source[cursor..].find('<') else {
                let parent = *stack.last().unwrap_or(&self.root);
                self.append_text(parent, &source[cursor..]);
                break;
            };
            let open = cursor + relative_open;
            if open > cursor {
                let parent = *stack.last().unwrap_or(&self.root);
                self.append_text(parent, &source[cursor..open]);
            }
            if source[open..].starts_with("<!--") {
                if let Some(relative_end) = source[open + 4..].find("-->") {
                    let end = open + 4 + relative_end;
                    let parent = *stack.last().unwrap_or(&self.root);
                    self.append_node(parent, NodeKind::Comment(source[open + 4..end].to_owned()));
                    cursor = end + 3;
                    continue;
                }
                let parent = *stack.last().unwrap_or(&self.root);
                self.append_text(parent, &source[open..]);
                break;
            }
            let Some(relative_end) = find_tag_end(&source, open + 1) else {
                let parent = *stack.last().unwrap_or(&self.root);
                self.append_text(parent, &source[open..]);
                break;
            };
            let end = open + 1 + relative_end;
            let token = source[open + 1..end].trim();
            if let Some(closing_name) = token.strip_prefix('/') {
                let name = parse_name(closing_name);
                if let Some(position) = stack.iter().rposition(|id| {
                    matches!(
                        &self.nodes[id.0].kind,
                        NodeKind::Element { tag_name, .. } if tag_name == &name
                    )
                }) {
                    stack.truncate(position);
                    if stack.is_empty() {
                        stack.push(self.root);
                    }
                }
            } else if !token.starts_with('!') && !token.starts_with('?') && !token.is_empty() {
                let (name, attributes, self_closing) = parse_start_tag(token);
                if !name.is_empty() {
                    let parent = *stack.last().unwrap_or(&self.root);
                    let id = self.append_node(
                        parent,
                        NodeKind::Element { tag_name: name.clone(), attributes },
                    );
                    if !self_closing && !is_void_element(&name) {
                        stack.push(id);
                    }
                }
            }
            cursor = end + 1;
        }
        self.source = source;
    }

    fn append_text(&mut self, parent: NodeId, text: &str) {
        if !text.is_empty() {
            self.append_node(parent, NodeKind::Text(text.to_owned()));
        }
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
                value = String::from_utf8_lossy(&bytes[value_start..index]).into_owned();
                if index < bytes.len() {
                    index += 1;
                }
            } else {
                let value_start = index;
                while index < bytes.len() && !bytes[index].is_ascii_whitespace() {
                    index += 1;
                }
                value = String::from_utf8_lossy(&bytes[value_start..index]).into_owned();
            }
        }
        if !attributes.iter().any(|attribute: &Attribute| attribute.name == attr_name) {
            attributes.push(Attribute { name: attr_name, value });
        }
    }
    (name, attributes, self_closing)
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

/// Loads documents and their external resources using one guarded transport.
///
/// This type deliberately has no reqwest client and no alternate networking
/// implementation. Its only network capability is the supplied HttpTransport.
pub struct DocumentLoader {
    transport: HttpTransport,
}

impl DocumentLoader {
    #[must_use]
    pub const fn new(transport: HttpTransport) -> Self {
        Self { transport }
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
        let source = read_bounded(response, MAX_DOCUMENT_BYTES).map_err(|error| match error {
            BoundedReadError::Io(error) => DocumentLoadError::Io(error),
            BoundedReadError::TooLarge => DocumentLoadError::TooLarge { limit: MAX_DOCUMENT_BYTES },
        })?;
        let source = String::from_utf8(source).map_err(|_| DocumentLoadError::InvalidUtf8)?;
        let document = HtmlDocument::parse(source);
        let references = document.resource_references();
        let mut resources = Vec::new();
        let mut failed_resources = Vec::new();

        for reference in references {
            let resolved = match url.join(&reference.url) {
                Ok(resolved) if matches!(resolved.scheme(), "http" | "https") => resolved,
                Ok(_) => {
                    failed_resources.push(FailedResource {
                        reference,
                        error: "unsupported resource URL scheme".to_owned(),
                    });
                    continue;
                }
                Err(error) => {
                    failed_resources.push(FailedResource { reference, error: error.to_string() });
                    continue;
                }
            };
            let response = self.transport.execute(
                "GET",
                resolved.as_str(),
                url.as_str(),
                reference.kind.network_type(),
                HeaderMap::new(),
                None,
            );
            let response = match response {
                Ok(response) if response.status().is_success() => response,
                Ok(response) => {
                    failed_resources.push(FailedResource {
                        reference,
                        error: format!("HTTP status {}", response.status().as_u16()),
                    });
                    continue;
                }
                Err(error) => {
                    failed_resources.push(FailedResource { reference, error: error.to_string() });
                    continue;
                }
            };
            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let limit = reference.kind.max_bytes();
            match read_bounded(response, limit) {
                Ok(bytes) => resources.push(LoadedResource {
                    url: resolved,
                    kind: reference.kind,
                    content_type,
                    bytes,
                }),
                Err(BoundedReadError::Io(error)) => {
                    failed_resources.push(FailedResource { reference, error: error.to_string() })
                }
                Err(BoundedReadError::TooLarge) => failed_resources.push(FailedResource {
                    reference,
                    error: format!("resource exceeded the {limit}-byte limit"),
                }),
            }
        }

        Ok(LoadedDocument { url, document, resources, failed_resources })
    }
}

enum BoundedReadError {
    Io(io::Error),
    TooLarge,
}

fn read_bounded(reader: impl Read, limit: usize) -> Result<Vec<u8>, BoundedReadError> {
    let mut bytes = Vec::new();
    reader
        .take(u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(BoundedReadError::Io)?;
    if bytes.len() > limit {
        return Err(BoundedReadError::TooLarge);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::{HtmlDocument, NodeKind, ResourceKind};

    #[test]
    fn builds_parent_child_relationships_in_an_arena() {
        let document = HtmlDocument::parse("<html><body><p>Hello</p></body></html>");
        assert_eq!(document.node_count(), 5);
        let root = document.node(document.root()).expect("document root");
        assert_eq!(root.children().len(), 1);
        let html = document.node(root.children()[0]).expect("html node");
        assert!(matches!(html.kind(), NodeKind::Element { tag_name, .. } if tag_name == "html"));
        assert_eq!(html.parent(), Some(document.root()));
    }

    #[test]
    fn extracts_inline_stylesheets_in_document_order() {
        let document = HtmlDocument::parse(
            "<html><head><style>body { color: red; }</style><style>p { margin: 0; }</style></head></html>",
        );

        assert_eq!(
            document.inline_stylesheets(),
            vec!["body { color: red; }", "p { margin: 0; }"]
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
}
