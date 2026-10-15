//! Utility functions for working with ropes and positions.

pub use kcl_lib::IntoDiagnostic;
pub(super) use kcl_lib::byte_to_lsp_position as byte_to_position;
use ropey::Rope;
use tower_lsp::lsp_types::Position;

/// Split a token into single-line UTF-16 spans for clients without multiline
/// semantic token support.
pub(super) fn semantic_token_spans(source: &str, start: usize, end: usize) -> Vec<(Position, u32)> {
    let Some(text) = source.get(start..end) else {
        return vec![];
    };
    let mut offset = start;
    let mut spans = vec![];
    for piece in text.split_inclusive(['\r', '\n']) {
        let text = piece.trim_end_matches(['\r', '\n']);
        if !text.is_empty()
            && let Some(position) = byte_to_position(source, offset)
        {
            spans.push((position, text.encode_utf16().count() as u32));
        }
        offset += piece.len();
    }
    spans
}

pub fn position_to_offset(position: Position, rope: &Rope) -> Option<usize> {
    let byte = kcl_lib::lsp_position_to_byte(&rope.to_string(), position)?;
    rope.try_byte_to_char(byte).ok()
}

pub fn get_text_before(offset: usize, rope: &Rope) -> Option<String> {
    if offset == 0 {
        return Some("".to_string());
    }
    Some(rope.get_slice(0..offset)?.to_string())
}

pub fn get_text_after(offset: usize, rope: &Rope) -> Option<String> {
    let end_idx = rope.len_chars();
    if offset == end_idx {
        return Some("".to_string());
    }
    Some(rope.get_slice(offset..end_idx)?.to_string())
}

pub fn get_line_before(pos: Position, rope: &Rope) -> Option<String> {
    let source = rope.to_string();
    let offset = kcl_lib::lsp_position_to_byte(&source, pos)?;
    let line_start = kcl_lib::lsp_position_to_byte(&source, Position::new(pos.line, 0))?;
    Some(source.get(line_start..offset)?.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_copilot_prefixes_and_semantic_spans() {
        let rope = Rope::from_str("é😀\r\nx");
        assert_eq!(position_to_offset(Position::new(0, 3), &rope), Some(2));
        assert_eq!(get_line_before(Position::new(0, 3), &rope).as_deref(), Some("é😀"));
        assert_eq!(get_line_before(Position::new(0, 2), &rope), None);
        assert_eq!(get_line_before(Position::new(99, 0), &rope), None);
        assert_eq!(get_text_before(99, &rope), None);
        assert_eq!(
            get_line_before(Position::new(1, 1), &Rope::from_str("a\u{2028}b\nc")).as_deref(),
            Some("c")
        );
        assert_eq!(
            semantic_token_spans("é😀\r\nx", 0, 9),
            vec![(Position::new(0, 0), 3), (Position::new(1, 0), 1),]
        );
    }
}
