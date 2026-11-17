//! Converts computed `::before`/`::after` styles into layout content.

use std::{collections::HashMap, sync::Arc};

use bweb_css::{ComputedStyle, PseudoElement};
use bweb_html::NodeId;
use bweb_layout::{GeneratedText, PseudoContent};

/// Builds the per-element generated content consumed by the layout engine.
pub fn pseudo_content(
    styles: &HashMap<(NodeId, PseudoElement), ComputedStyle>,
) -> HashMap<NodeId, PseudoContent> {
    let mut result = HashMap::new();
    for ((id, element), style) in styles {
        let Some(text) = style.get("content").map(unquote).filter(|text| !text.is_empty()) else {
            continue;
        };
        let generated = GeneratedText { text, style: Arc::new(style.clone()) };
        let entry: &mut PseudoContent = result.entry(*id).or_default();
        match element {
            PseudoElement::Before => entry.before = Some(generated),
            PseudoElement::After => entry.after = Some(generated),
        }
    }
    result
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    let bytes = value.as_bytes();
    if bytes.len() >= 2 && matches!(bytes[0], b'"' | b'\'') && bytes[bytes.len() - 1] == bytes[0] {
        value[1..value.len() - 1].to_owned()
    } else {
        value.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::{pseudo_content, unquote};
    use bweb_css::{StyleEngine, Stylesheet};
    use bweb_html::{HtmlDocument, NodeKind};

    #[test]
    fn strips_quotes_from_content_values() {
        assert_eq!(unquote("  \"hello\" "), "hello");
        assert_eq!(unquote("'x'"), "x");
        assert_eq!(unquote("counter(x)"), "counter(x)");
    }

    #[test]
    fn maps_pseudo_styles_to_before_and_after_content() {
        let document = HtmlDocument::parse("<p>x</p>");
        let sheet = Stylesheet::parse(
            "p::before { content: \"→\"; color: red } p::after { content: \"!\" }",
        )
        .expect("stylesheet");
        let (_, generated) =
            StyleEngine::new().compute_generated(&document, &[sheet], 800.0, 600.0);
        let id = document
            .iter()
            .find_map(|(id, node)| {
                matches!(node.kind(), NodeKind::Element { tag_name, .. } if tag_name == "p")
                    .then_some(id)
            })
            .expect("paragraph");

        let content = pseudo_content(&generated);
        let entry = content.get(&id).expect("generated content");
        assert_eq!(entry.before.as_ref().expect("before").text, "→");
        assert_eq!(entry.after.as_ref().expect("after").text, "!");
        assert_eq!(entry.before.as_ref().expect("before").style.get("color"), Some("red"));
    }
}
