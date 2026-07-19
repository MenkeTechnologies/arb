//! `SpecError` — a lex/parse/build error that optionally carries the source
//! span (char offsets) where it occurred, so the LSP can anchor a diagnostic to
//! the real location. It converts freely to/from `String` (via the `From` impls
//! below), so every existing caller that used `Result<_, String>` keeps working:
//! `?` widens a `String` error into a spanless `SpecError`, and a `SpecError`
//! narrows back to its message for anything that still wants a `String`.

use std::fmt;

/// An error with an optional `(start, end)` char-offset span in the source.
#[derive(Debug, Clone)]
pub struct SpecError {
    pub msg: String,
    pub span: Option<(usize, usize)>,
}

impl SpecError {
    /// Attach a span if none is set yet (keeps a more precise inner span).
    pub fn or_span(mut self, start: usize, len: usize) -> Self {
        if self.span.is_none() {
            self.span = Some((start, start + len));
        }
        self
    }
}

impl fmt::Display for SpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl From<String> for SpecError {
    fn from(msg: String) -> Self {
        SpecError { msg, span: None }
    }
}

impl From<&str> for SpecError {
    fn from(msg: &str) -> Self {
        SpecError {
            msg: msg.to_string(),
            span: None,
        }
    }
}

impl From<&String> for SpecError {
    fn from(msg: &String) -> Self {
        SpecError {
            msg: msg.clone(),
            span: None,
        }
    }
}

impl From<SpecError> for String {
    fn from(e: SpecError) -> String {
        e.msg
    }
}
