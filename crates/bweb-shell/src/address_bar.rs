//! Editable address-bar state for the shell window.

const MAX_TEXT_LENGTH: usize = 2048;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressBar {
    text: String,
}

impl AddressBar {
    #[must_use]
    pub fn new(url: &str) -> Self {
        Self { text: url.to_owned() }
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn set_url(&mut self, url: &str) {
        self.text.clear();
        self.text.push_str(url);
    }

    pub fn clear(&mut self) {
        self.text.clear();
    }

    pub fn insert(&mut self, character: char) {
        if self.text.len() < MAX_TEXT_LENGTH {
            self.text.push(character);
        }
    }

    pub fn backspace(&mut self) {
        self.text.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::AddressBar;

    #[test]
    fn edits_and_url_replacement() {
        let mut bar = AddressBar::new("https://example.test/");
        bar.insert('x');
        assert_eq!(bar.text(), "https://example.test/x");
        bar.backspace();
        assert_eq!(bar.text(), "https://example.test/");
        bar.set_url("https://other.test/");
        assert_eq!(bar.text(), "https://other.test/");
        bar.clear();
        assert_eq!(bar.text(), "");
    }
}
