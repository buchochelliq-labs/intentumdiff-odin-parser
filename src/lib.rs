//! Odin parser plugin — full-parse mode.
//!
//! Handles `.odin` files.
//! The plugin parses source with Tree-sitter inside Rust/Wasm.
//!
//! Key design decisions:
//! - Odin is a C-like systems language.  Procedures map to method-like nodes;
//!   struct, enum, union, and bit_field declarations map to class-like nodes.
//! - `package_declaration` is surfaced so package-level renames are tracked.
//! - `foreign_block` is treated as a class-like container (groups foreign imports).
//! - `when_declaration` (compile-time when) is a passthrough — its children are
//!   surfaced as if they appeared directly at the enclosing scope.
//! - Constant declarations whose rhs is a struct/enum literal are also surfaced as
//!   class-like (Odin idiom: `MyStruct :: struct { ... }`).

use intentumdiff_plugin_sdk::{
    cst::CstNode,
    hash::structural_hash_with_memo,
    tree::{SemanticNode, SemanticNodeBuilder},
};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentumdiff::plugin::parser::ExamplePair;
use crate::exports::intentumdiff::plugin::parser::Guest;
use crate::exports::intentumdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentumdiff::plugin::parser::ParserMode;

const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}
struct OdinParser;

const TRIVIA: &[&str] = &["comment", "line_comment", "block_comment"];

fn is_trivia(node_type: &str) -> bool {
    TRIVIA.contains(&node_type)
}

fn is_class_like(node_type: &str) -> bool {
    matches!(
        node_type,
        "struct_declaration"
            | "struct_type"
            | "enum_declaration"
            | "enum_type"
            | "union_declaration"
            | "union_type"
            | "bit_field_declaration"
            | "bit_field_type"
            | "foreign_block"
            | "foreign_declaration"
            | "package_declaration"
            | "package_clause"
    )
}

fn is_method_like(node_type: &str) -> bool {
    matches!(
        node_type,
        "procedure_declaration"
            | "proc_declaration"
            | "procedure_definition"
            | "procedure_literal"
            | "procedure_type"
    )
}

/// Check if a constant/variable declaration's rhs is a struct/enum/union literal,
/// making it a class-like node.
fn rhs_is_type_literal(node: &CstNode) -> bool {
    for child in &node.children {
        if matches!(
            child.node_type.as_str(),
            "struct_type"
                | "enum_type"
                | "union_type"
                | "bit_field_type"
                | "struct_declaration"
                | "enum_declaration"
                | "union_declaration"
        ) {
            return true;
        }
    }
    false
}

/// Check if a constant declaration's rhs is a procedure literal.
fn rhs_is_procedure(node: &CstNode) -> bool {
    for child in &node.children {
        if matches!(
            child.node_type.as_str(),
            "procedure_literal" | "procedure_type" | "procedure_declaration"
        ) {
            return true;
        }
    }
    false
}

/// Extract the declared name from a declaration node.
fn label_for(node: &CstNode) -> String {
    for child in &node.children {
        if is_trivia(&child.node_type) {
            continue;
        }
        // Skip keywords and punctuation
        if matches!(
            child.node_type.as_str(),
            "struct"
                | "enum"
                | "union"
                | "proc"
                | "package"
                | "foreign"
                | "::"
                | ":"
                | "="
                | "->"
                | "bit_field"
        ) {
            continue;
        }
        if matches!(
            child.node_type.as_str(),
            "identifier"
                | "qualified_identifier"
                | "package_identifier"
                | "type_identifier"
                | "name"
        ) {
            return child.text.as_deref().unwrap_or("").to_string();
        }
    }
    "(anonymous)".to_string()
}

fn convert(
    node: &CstNode,
    id_prefix: &str,
    current_class: Option<&str>,
    memo: &mut std::collections::HashMap<usize, String>,
) -> Option<SemanticNode> {
    if is_trivia(&node.node_type) {
        return None;
    }

    let hash = structural_hash_with_memo(node, memo);

    // Constant/variable declarations that define types (MyStruct :: struct { ... })
    // Odin uses `declaration_statement` or `const_declaration` for these.
    let is_typed_const = matches!(
        node.node_type.as_str(),
        "const_declaration"
            | "variable_declaration"
            | "declaration_statement"
            | "value_declaration"
    ) && rhs_is_type_literal(node);

    let is_proc_const = matches!(
        node.node_type.as_str(),
        "const_declaration"
            | "variable_declaration"
            | "declaration_statement"
            | "value_declaration"
    ) && rhs_is_procedure(node);

    if is_class_like(&node.node_type) || is_typed_const {
        let label = label_for(node);
        let children: Vec<SemanticNode> = node
            .children
            .iter()
            .enumerate()
            .filter_map(|(i, c)| convert(c, &format!("{}.{}", id_prefix, i), Some(&label), memo))
            .collect();
        let builder = SemanticNodeBuilder::new(
            id_prefix,
            node.node_type.clone(),
            label,
            node.start_line,
            node.start_col,
            node.end_line,
            node.end_col,
            hash,
        );
        return Some(builder.children(children).build());
    }

    if is_method_like(&node.node_type) || is_proc_const {
        let label = label_for(node);
        let children: Vec<SemanticNode> = node
            .children
            .iter()
            .enumerate()
            .filter_map(|(i, c)| convert(c, &format!("{}.{}", id_prefix, i), current_class, memo))
            .collect();
        let mut builder = SemanticNodeBuilder::new(
            id_prefix,
            node.node_type.clone(),
            label,
            node.start_line,
            node.start_col,
            node.end_line,
            node.end_col,
            hash,
        );
        builder = builder.children(children);
        if let Some(cls) = current_class {
            builder = builder.parent_type(cls.to_string());
        }
        return Some(builder.build());
    }

    // #46: literal leaves are review content — the passthrough dropped them, so a
    // numeric bump (`add(3, 4)` -> `add(4, 4)`) or a string edit hashed style-only.
    if node.children.is_empty()
        && intentumdiff_plugin_sdk::ts_convert::cst_literal_kind(&node.node_type)
    {
        let text: String = node.text_or_empty().trim().chars().take(120).collect();
        if !text.is_empty() {
            return Some(
                SemanticNodeBuilder::new(
                    id_prefix,
                    node.node_type.clone(),
                    text,
                    node.start_line,
                    node.start_col,
                    node.end_line,
                    node.end_col,
                    hash,
                )
                .build(),
            );
        }
    }

    // `when_declaration` — passthrough (compile-time conditional block)
    // Also passthrough for all other non-semantic containers.
    let children: Vec<SemanticNode> = node
        .children
        .iter()
        .enumerate()
        .filter_map(|(i, c)| convert(c, &format!("{}.{}", id_prefix, i), current_class, memo))
        .collect();
    if children.len() == 1 {
        return Some(children.into_iter().next().unwrap());
    }
    if !children.is_empty() {
        let builder = SemanticNodeBuilder::new(
            id_prefix,
            node.node_type.clone(),
            String::new(),
            node.start_line,
            node.start_col,
            node.end_line,
            node.end_col,
            hash,
        );
        return Some(builder.children(children).build());
    }
    None
}

fn node_to_cst(node: tree_sitter::Node<'_>, source: &[u8]) -> CstNode {
    let children: Vec<CstNode> = (0..node.named_child_count())
        .filter_map(|i| node.named_child(i))
        .map(|child| node_to_cst(child, source))
        .collect();

    let text = if children.is_empty() {
        Some(
            node.utf8_text(source)
                .unwrap_or("")
                .chars()
                .take(4096)
                .collect(),
        )
    } else {
        None
    };

    CstNode {
        node_type: node.kind().to_string(),
        named: node.is_named(),
        text,
        start_line: node.start_position().row as u32,
        start_col: node.start_position().column as u32,
        end_line: node.end_position().row as u32,
        end_col: node.end_position().column as u32,
        children,
    }
}

fn parse_source(source: &str) -> Result<CstNode, String> {
    let mut parser = tree_sitter::Parser::new();
    let lang = tree_sitter_odin::LANGUAGE.into();
    parser
        .set_language(&lang)
        .map_err(|_| "Failed to load odin grammar".to_string())?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "Parse failed".to_string())?;
    Ok(node_to_cst(tree.root_node(), source.as_bytes()))
}

fn process_impl(source: &str) -> String {
    let root: CstNode = match parse_source(source) {
        Ok(n) => n,
        Err(e) => return format!(r#"{{\"error\":\"{}\"}}"#, e),
    };
    let mut memo = std::collections::HashMap::new();
    let sem = match convert(&root, "0", None, &mut memo) {
        Some(n) => n,
        None => return r#"{"error":"Empty semantic tree"}"#.to_string(),
    };
    match serde_json::to_string(&sem) {
        Ok(s) => s,
        Err(e) => format!(r#"{{"error":"Serialisation error: {}"}}"#, e),
    }
}

impl Guest for OdinParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }
    fn grammar_id() -> String {
        "odin".to_string()
    }
    fn detect_language(filename: String, _content: String) -> String {
        if filename.to_lowercase().ends_with(".odin") {
            return "odin".to_string();
        }
        String::new()
    }
    fn preprocess_source(source: String) -> String {
        source
    }
    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: "package main\n\nimport \"core:fmt\"\n\ngreet :: proc(name: string) {\n    fmt.printf(\"Hello, %s\\n\", name)\n}\n\nadd :: proc(a, b: int) -> int {\n    return a + b\n}\n\nmain :: proc() {\n    greet(\"World\")\n    fmt.println(add(3, 4))\n}\n".to_string(),
            new: "package main\n\nimport \"core:fmt\"\n\ngreet :: proc(name: string) {\n    fmt.printf(\"Hello, %s!\\n\", name)\n}\n\nadd :: proc(a, b: int) -> int { return a + b }\n\nmultiply :: proc(a, b: int) -> int { return a * b }\n\nmain :: proc() {\n    greet(\"World\")\n    fmt.println(\"add:\", add(3, 4))\n    fmt.println(\"mul:\", multiply(3, 4))\n}\n".to_string(),
        }
    }
    fn process(input: String, _language: String, _filename: String) -> String {
        process_impl(&input)
    }
    fn trivia_node_types() -> Vec<String> {
        TRIVIA.iter().map(|s| s.to_string()).collect()
    }
    fn language_ids() -> Vec<String> {
        vec!["odin".to_string()]
    }
    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }
    fn priority() -> i32 {
        0
    }
}

export!(OdinParser);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exports::intentumdiff::plugin::parser::Guest;
    use intentumdiff_plugin_sdk::testing as t;

    #[test]
    fn grammar_id_nonempty() {
        assert!(!OdinParser::grammar_id().is_empty());
    }

    #[test]
    fn language_ids_contain_grammar_id() {
        let gid = OdinParser::grammar_id();
        let ids = OdinParser::language_ids();
        assert!(
            ids.contains(&gid),
            "language_ids {:?} must contain {:?}",
            ids,
            gid
        );
    }

    #[test]
    fn detect_language_known_ext() {
        let r = OdinParser::detect_language("test.odin".to_string(), "".to_string());
        assert_eq!(r.as_str(), "odin");
    }

    #[test]
    fn detect_language_unknown_ext() {
        let r =
            OdinParser::detect_language("test.xyz_notareal_ext_9z8y".to_string(), "".to_string());
        assert_eq!(r.as_str(), "");
    }

    #[test]
    fn parser_mode_is_full_parse() {
        assert!(matches!(
            OdinParser::get_parser_mode(),
            ParserMode::FullParse
        ));
    }

    #[test]
    fn process_impl_accepts_raw_example_source() {
        let example = OdinParser::example(OdinParser::grammar_id());
        let out = process_impl(&example.old);
        t::assert_valid_json(&out, "process(raw example)");
        assert!(!out.contains("\"error\""), "{out}");
    }
    #[test]
    fn process_impl_empty_returns_valid_json() {
        let out = process_impl("");
        t::assert_valid_json(&out, "process(empty)");
    }

    #[test]
    fn process_impl_whitespace_returns_valid_json() {
        let out = process_impl("   \n  ");
        t::assert_valid_json(&out, "process(whitespace)");
    }
}
