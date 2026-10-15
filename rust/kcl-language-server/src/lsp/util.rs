//! Utility functions for working with ropes and positions.

pub use kcl_lib::IntoDiagnostic;
use ropey::Rope;
use tower_lsp::lsp_types::Position;

/// Convert a UTF-8 source offset to the UTF-16 position used by LSP.
pub(super) fn byte_to_position(source: &str, offset: usize) -> Option<Position> {
    let prefix = source.get(..offset)?;
    Some(Position::new(
        prefix.bytes().filter(|byte| *byte == b'\n').count() as u32,
        prefix.rsplit('\n').next()?.encode_utf16().count() as u32,
    ))
}

pub fn position_to_offset(position: Position, rope: &Rope) -> Option<usize> {
    Some(rope.try_line_to_char(position.line as usize).ok()? + position.character as usize)
}

pub fn get_text_before(offset: usize, rope: &Rope) -> Option<String> {
    if offset == 0 {
        return Some("".to_string());
    }
    Some(rope.slice(0..offset).to_string())
}

pub fn get_text_after(offset: usize, rope: &Rope) -> Option<String> {
    let end_idx = rope.len_chars();
    if offset == end_idx {
        return Some("".to_string());
    }
    Some(rope.slice(offset..end_idx).to_string())
}

pub fn get_line_before(pos: Position, rope: &Rope) -> Option<String> {
    let char_offset = pos.character as usize;
    let offset = position_to_offset(pos, rope).unwrap();
    if char_offset == 0 {
        return Some("".to_string());
    }
    let line_start = offset - char_offset;
    Some(rope.slice(line_start..offset).to_string())
}
