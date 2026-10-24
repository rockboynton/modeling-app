//! Bounded, position-preserving recovery for read-only editor requests.
//! Diagnostics and rename always use the original, strictly parsed source.
use crate::Program;
use crate::parsing::token::adapter;

pub(super) fn source(source: &str) -> Option<String> {
    let mut working = source.to_owned();
    for _ in 0..8 {
        let tokens = adapter::lex_with_diagnostics(&working, Default::default()).tokens;
        let code: Vec<_> = tokens.iter().filter(|token| token.is_code_token()).collect();
        let mut closing = vec![];
        for token in &code {
            match &working[token.start..token.end] {
                "(" => closing.push(')'),
                "[" => closing.push(']'),
                "{" => closing.push('}'),
                ")" | "]" | "}" => {
                    closing.pop();
                }
                _ => {}
            }
        }
        let mut candidate = working.clone();
        candidate.push('\n');
        candidate.extend(closing.iter().rev());
        let error = match Program::parse_no_errs(&candidate) {
            Ok(_) => return Some(candidate),
            Err(error) => error,
        };
        if let Some(token) = code.last()
            && &working[token.start..token.end] == ","
        {
            working.replace_range(token.start..token.end, " ");
            continue;
        }
        let offset = error
            .source_ranges()
            .first()
            .map(|range| range.start())
            .unwrap_or(working.len())
            .min(working.len());
        // An unfinished expression can make the parser blame the next line.
        // Prefer the closest preceding line ending in an incomplete operator.
        let suspicious = code.iter().rev().find(|token| {
            token.end <= offset
                && matches!(
                    &working[token.start..token.end],
                    "=" | "." | "::" | "|>" | "+" | "-" | "*" | "/"
                )
                && working[token.end..]
                    .split('\n')
                    .next()
                    .is_some_and(|rest| rest.trim().is_empty())
        });
        let offset = suspicious.map(|token| token.start).unwrap_or_else(|| {
            code.iter()
                .rev()
                .find(|token| token.start < offset)
                .map(|token| token.start)
                .unwrap_or(offset)
        });
        let start = working.as_bytes()[..offset]
            .iter()
            .rposition(|&b| b == b'\n')
            .map(|at| at + 1)
            .unwrap_or(0);
        let end = working.as_bytes()[offset..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|at| offset + at)
            .unwrap_or(working.len());
        // Removing a scope boundary could silently bind names in the wrong
        // scope. Decline recovery rather than guessing in that case.
        if code.iter().any(|token| {
            start <= token.start && token.start < end && matches!(&working[token.start..token.end], "{" | "}")
        }) {
            return None;
        }
        if working[start..end].trim().is_empty() {
            return None;
        }
        let masked: String = working[start..end]
            .chars()
            .map(|c| {
                if c == '\r' {
                    "\r".into()
                } else {
                    " ".repeat(c.len_utf8())
                }
            })
            .collect();
        working.replace_range(start..end, &masked);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_balances_calls_and_masks_broken_lines_without_moving_bindings() {
        for source in [
            "@settings(kclVersion = 3.0)\nfn twice(@x) { return x * 2 }\nvalue = twice(",
            "@settings(kclVersion = 3.0)\nfn twice(@x) { return x * 2 }\nvalue = twice(2,",
            "@settings(kclVersion = 3.0)\nwidth = 2mm\nbroken =\nresult = width\n",
            "@settings(kclVersion = 3.0)\nfn f(@x) {\nlocal =\n",
        ] {
            let recovered = super::source(source).unwrap_or_else(|| panic!("{source}"));
            Program::parse_no_errs(&recovered).unwrap();
            if let Some(offset) = source.find("result") {
                assert_eq!(recovered.find("result"), Some(offset));
            }
        }
        assert!(super::source("fn f() { invalid = }").is_none());
    }

    #[tokio::test]
    async fn editor_recovery_keeps_hover_signatures_and_completion_but_not_rename() {
        use tower_lsp::LanguageServer;
        use tower_lsp::lsp_types::*;
        let server = crate::lsp::test_util::kcl_lsp_server(false).await.unwrap();
        let uri = Url::parse("file:///recover-editor.kcl").unwrap();
        let source = "@settings(kclVersion = 3.0)\n/// Doubles a value.\nfn twice(@x: number(mm)) { return x * 2 }\nbroken =\nresult = twice(2mm)\nlast = twice(";
        server
            .did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "kcl".into(),
                    version: 1,
                    text: source.into(),
                },
            })
            .await;
        let position = TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position: kcl_lib::byte_to_lsp_position(source, source.len()).unwrap(),
        };
        assert!(server.source_signature_help(&position).await.is_some());
        let selected = TextDocumentPositionParams {
            position: kcl_lib::byte_to_lsp_position(source, source.find("twice(2").unwrap() + 1).unwrap(),
            ..position
        };
        assert!(server.source_hover(&selected).await.is_some());
        assert!(
            server
                .lexical_completions(&uri, source, source.find("twice(2").unwrap() + 2)
                .await
                .unwrap()
                .items
                .iter()
                .any(|item| item.label == "twice")
        );
        assert!(server.navigation_index_with_errors(&uri, false).await.1.contains(&uri));
        assert_eq!(server.code_map.get(uri.as_str()).unwrap().as_slice(), source.as_bytes());
        assert!(!server.diagnostics_map.get(uri.as_str()).unwrap().is_empty());
    }
}
