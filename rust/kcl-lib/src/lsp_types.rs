//! Conversions between KCL source data and Language Server Protocol types.

use tower_lsp::lsp_types::Diagnostic;
use tower_lsp::lsp_types::DiagnosticSeverity;
use tower_lsp::lsp_types::DiagnosticTag;
use tower_lsp::lsp_types::Position;
use tower_lsp::lsp_types::Range;

use crate::CompilationIssue;
use crate::errors::Severity;
use crate::errors::Suggestion;
use crate::errors::Tag;

/// Convert an object into LSP diagnostics for a source document.
pub trait IntoDiagnostic {
    /// Convert the object to diagnostics. `uri` is the document under which
    /// the diagnostics are published.
    fn to_lsp_diagnostics(&self, text: &str, uri: &tower_lsp::lsp_types::Url) -> Vec<Diagnostic>;

    /// Get the severity of the diagnostic.
    fn severity(&self) -> DiagnosticSeverity;
}

impl IntoDiagnostic for CompilationIssue {
    fn to_lsp_diagnostics(&self, code: &str, _uri: &tower_lsp::lsp_types::Url) -> Vec<Diagnostic> {
        let edit = self.suggestion.as_ref().map(|s| to_lsp_edit(s, code));

        vec![Diagnostic {
            range: self.source_range.to_lsp_range(code),
            severity: Some(self.severity()),
            code: None,
            code_description: None,
            source: Some("kcl".to_string()),
            message: self.message.clone(),
            related_information: None,
            tags: tag_to_lsp_tags(self.tag),
            data: edit.map(|e| serde_json::to_value(e).unwrap()),
        }]
    }

    fn severity(&self) -> DiagnosticSeverity {
        match self.severity {
            Severity::Warning => DiagnosticSeverity::WARNING,
            _ => DiagnosticSeverity::ERROR,
        }
    }
}

fn tag_to_lsp_tags(tag: Tag) -> Option<Vec<DiagnosticTag>> {
    match tag {
        Tag::Deprecated => Some(vec![DiagnosticTag::DEPRECATED]),
        Tag::Unnecessary => Some(vec![DiagnosticTag::UNNECESSARY]),
        Tag::UnknownNumericUnits | Tag::None => None,
    }
}

pub type LspSuggestion = (Suggestion, tower_lsp::lsp_types::Range);

pub(crate) fn to_lsp_edit(suggestion: &Suggestion, code: &str) -> LspSuggestion {
    let range = suggestion.source_range.to_lsp_range(code);
    (suggestion.clone(), range)
}

pub trait ToLspRange {
    fn to_lsp_range(&self, code: &str) -> Range {
        let start = self.start_to_lsp_position(code);
        let end = self.end_to_lsp_position(code);
        Range { start, end }
    }

    fn start_to_lsp_position(&self, code: &str) -> Position;
    fn end_to_lsp_position(&self, code: &str) -> Position;
}

// Exclude line terminators while preserving an empty final line. CRLF is one
// line break; LSP also accepts lone CR and LF.
fn source_lines(source: &str) -> impl Iterator<Item = (usize, usize)> + '_ {
    let mut start = 0;
    std::iter::from_fn(move || {
        let tail = source.get(start..)?;
        let end = start + tail.find(['\r', '\n']).unwrap_or(tail.len());
        let line = (start, end);
        start = if source.as_bytes().get(end..end + 2) == Some(b"\r\n") {
            end + 2
        } else {
            end + 1
        };
        Some(line)
    })
}

/// Convert an LSP UTF-16 position into a UTF-8 source offset.
///
/// Columns beyond the line end are clamped as required by LSP. Positions in the
/// middle of a surrogate pair or outside the document return `None`.
pub fn lsp_position_to_byte(source: &str, position: Position) -> Option<usize> {
    let (start, end) = source_lines(source).nth(position.line as usize)?;
    let mut units = 0;
    for (byte, ch) in source[start..end].char_indices() {
        if units == position.character {
            return Some(start + byte);
        }
        units += ch.len_utf16() as u32;
        if units > position.character {
            return None;
        }
    }
    Some(end)
}

/// Convert a UTF-8 source offset into an LSP UTF-16 position.
/// Invalid UTF-8 boundaries and offsets within a CRLF terminator return `None`.
pub fn byte_to_lsp_position(source: &str, offset: usize) -> Option<Position> {
    source.get(..offset)?;
    for (line, (start, end)) in source_lines(source).enumerate() {
        if start <= offset && offset <= end {
            return Some(Position::new(
                line as u32,
                source[start..offset].encode_utf16().count() as u32,
            ));
        }
    }
    None
}

impl ToLspRange for crate::SourceRange {
    fn start_to_lsp_position(&self, code: &str) -> Position {
        byte_to_lsp_position(code, self.start()).unwrap_or_default()
    }

    fn end_to_lsp_position(&self, code: &str) -> Position {
        byte_to_lsp_position(code, self.end()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsp_positions_round_trip_unicode_and_line_endings() {
        for source in ["", "aé😀z", "aé😀\r\nb\rç\n", "\n", "\r\n"] {
            for offset in 0..=source.len() {
                if let Some(position) = byte_to_lsp_position(source, offset) {
                    assert_eq!(
                        lsp_position_to_byte(source, position),
                        Some(offset),
                        "{source:?} at {offset}"
                    );
                }
            }
        }
        assert_eq!(byte_to_lsp_position("é😀x", 6), Some(Position::new(0, 3)));
        assert_eq!(lsp_position_to_byte("é😀x", Position::new(0, 2)), None);
        assert_eq!(lsp_position_to_byte("é😀x", Position::new(0, 99)), Some(7));
        assert_eq!(lsp_position_to_byte("x", Position::new(1, 0)), None);
    }

    #[test]
    fn lsp_source_ranges_preserve_line_starts_and_eof() {
        let source = "é😀\r\nwidth\n";
        let start = source.find("width").unwrap();
        assert_eq!(
            crate::SourceRange::new(start, source.len(), Default::default()).to_lsp_range(source),
            Range::new(Position::new(1, 0), Position::new(2, 0))
        );
    }
}
