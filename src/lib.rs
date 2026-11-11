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

use crate::exports::intentdiff::plugin::parser::ExamplePair;
use crate::exports::intentdiff::plugin::parser::Guest;
use crate::exports::intentdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentdiff::plugin::parser::ParserMode;

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
    parent_type: Option<&str>,
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
            .filter_map(|(i, c)| {
                convert(
                    c,
                    &format!("{}.{}", id_prefix, i),
                    Some(&label),
                    Some(&node.node_type),
                    memo,
                )
            })
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
            .filter_map(|(i, c)| {
                convert(
                    c,
                    &format!("{}.{}", id_prefix, i),
                    current_class,
                    Some(&node.node_type),
                    memo,
                )
            })
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

    // odin-parser#4: an anonymous operator token is review content. Emit it as an
    // `operator` leaf labelled with the exact token, at the token's own span.
    if !node.named
        && node.children.is_empty()
        && parent_type.is_some_and(carries_operators)
        && is_operator_token(node.text_or_empty())
    {
        return Some(
            SemanticNodeBuilder::new(
                id_prefix,
                OPERATOR_NODE_TYPE.to_string(),
                node.text_or_empty().to_string(),
                node.start_line,
                node.start_col,
                node.end_line,
                node.end_col,
                hash,
            )
            .build(),
        );
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
        .filter_map(|(i, c)| {
            convert(
                c,
                &format!("{}.{}", id_prefix, i),
                current_class,
                Some(&node.node_type),
                memo,
            )
        })
        .collect();
    // Keep an operator's enclosing expression so the `operator` leaf stays under its
    // operator-bearing parent (the host's contract), rather than collapsing onto it.
    let has_operator = children.iter().any(|c| c.node_type == OPERATOR_NODE_TYPE);
    if children.len() == 1 && !has_operator {
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

/// Node type of the semantic leaf that carries one operator token; its label is the exact
/// token (`+`, `..<`, `or_else`). Matches the host contract in core's `operator_tokens.rs`
/// (core #229/#230), so the host recognises guest-emitted operators and skips its source
/// recovery.
const OPERATOR_NODE_TYPE: &str = "operator";

/// Odin's word-spelled operators (tree-sitter keeps them as anonymous tokens).
const WORD_OPERATORS: &[&str] = &[
    "in",
    "not_in",
    "or_else",
    "or_return",
    "or_break",
    "or_continue",
];

/// Range operators: `0..<n` and `0..=n` iterate different element counts.
const RANGE_OPERATORS: &[&str] = &["..<", "..=", ".."];

const OPERATOR_SYMBOLS: &[char] = &[
    '+', '-', '*', '/', '%', '<', '>', '=', '!', '&', '|', '^', '~', '?',
];

/// Is this anonymous token an operator that changes behaviour?
///
/// `->` (procedure result arrow) is declaration syntax, not an operator.
fn is_operator_token(token: &str) -> bool {
    if token.is_empty() || token == "->" {
        return false;
    }
    token.chars().all(|c| OPERATOR_SYMBOLS.contains(&c))
        || WORD_OPERATORS.contains(&token)
        || RANGE_OPERATORS.contains(&token)
}

/// Does *parent_type* carry its anonymous operator tokens as review content?
///
/// Declarations are excluded: their `=`/`::` never changes on its own (the declaration's
/// own node type and hash already capture it) and would only add noise.
fn carries_operators(parent_type: &str) -> bool {
    !(parent_type.contains("declaration") || parent_type.contains("definition"))
}

/// Rules whose statements are joined by the grammar's `_separator` (a hidden newline or
/// an explicit `;`). The newline form leaves no token, so the `;` form is dropped too.
const SEPARATED_RULES: &[&str] = &["source_file", "block", "switch_case", "switch_statement"];

/// Is *child* an optional statement separator `;` of *parent_kind* (odin-parser#6)?
///
/// Only the `_separator` positions qualify; `;` in `for`/`if` clauses and `bit_set[T; U]`
/// is required syntax and stays.
fn is_optional_separator(parent_kind: &str, child: tree_sitter::Node<'_>) -> bool {
    !child.is_named() && child.kind() == ";" && SEPARATED_RULES.contains(&parent_kind)
}

/// Tree-sitter → CstNode under the complete-token contract (odin-parser#4).
///
/// * Every token is kept, named AND anonymous (operators, keywords, qualifiers), so two
///   sources that differ only in an anonymous token never hash equal. The old converter
///   walked named children only, so `a + b` and `a - b` produced identical trees.
/// * Trivia (comments) is dropped here, so comment edits never reach structural hashes.
/// * Optional `;` statement separators are dropped, so `x := 1;` hashes like `x := 1`.
/// * Text-literal kinds (string/rune/number/float) stay leaves carrying their full text.
/// * Leaf text is the complete source payload: no length cap, so a long literal's tail
///   edit still changes the hash.
/// * Positions are 0-based rows and UTF-8 byte columns (tree-sitter's native units).
fn node_to_cst(node: tree_sitter::Node<'_>, source: &[u8]) -> CstNode {
    let literal = intentumdiff_plugin_sdk::ts_convert::cst_literal_kind(node.kind());
    let children: Vec<CstNode> = if literal {
        Vec::new()
    } else {
        let mut cursor = node.walk();
        node.children(&mut cursor)
            .filter(|child| !is_trivia(child.kind()))
            .filter(|child| !is_optional_separator(node.kind(), *child))
            .map(|child| node_to_cst(child, source))
            .collect()
    };

    let text = if children.is_empty() {
        Some(node.utf8_text(source).unwrap_or("").to_string())
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
    let sem = match convert(&root, "0", None, None, &mut memo) {
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
    use crate::exports::intentdiff::plugin::parser::Guest;
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

    // -----------------------------------------------------------------------
    // odin-parser#4: operator tokens survive the CST converter
    // -----------------------------------------------------------------------

    fn tree(src: &str) -> serde_json::Value {
        let out = process_impl(src);
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        assert!(v.get("error").is_none(), "{out}");
        v
    }

    fn collect<'a>(n: &'a serde_json::Value, ty: &str, out: &mut Vec<&'a serde_json::Value>) {
        if n["node_type"] == ty {
            out.push(n);
        }
        if let Some(cs) = n["children"].as_array() {
            for c in cs {
                collect(c, ty, out);
            }
        }
    }

    fn find_label<'a>(n: &'a serde_json::Value, label: &str) -> Option<&'a serde_json::Value> {
        if n["label"] == label {
            return Some(n);
        }
        n["children"]
            .as_array()?
            .iter()
            .find_map(|c| find_label(c, label))
    }

    fn operators(src: &str) -> Vec<(String, [u64; 4], String)> {
        let v = tree(src);
        let mut ops = Vec::new();
        collect(&v, OPERATOR_NODE_TYPE, &mut ops);
        ops.iter()
            .map(|o| {
                let p = &o["position"];
                (
                    o["label"].as_str().unwrap().to_string(),
                    [
                        p["start_line"].as_u64().unwrap(),
                        p["start_col"].as_u64().unwrap(),
                        p["end_line"].as_u64().unwrap(),
                        p["end_col"].as_u64().unwrap(),
                    ],
                    o["structural_hash"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect()
    }

    fn proc_hash(src: &str, name: &str) -> String {
        let v = tree(src);
        find_label(&v, name).expect("proc present")["structural_hash"]
            .as_str()
            .unwrap()
            .to_string()
    }

    const PLUS: &str = "package probe\ncombine :: proc(a, b: int) -> int { return a + b }\n";
    const MINUS: &str = "package probe\ncombine :: proc(a, b: int) -> int { return a - b }\n";

    #[test]
    fn body_operator_edit_is_a_real_change() {
        // Exact pair of core's `odin-body-operator` assessment case.
        let old = operators(PLUS);
        let new = operators(MINUS);
        assert_eq!(old.len(), 1, "{old:?}");
        assert_eq!(new.len(), 1, "{new:?}");
        assert_eq!(old[0].0, "+");
        assert_eq!(new[0].0, "-");
        // The token's own span: 0-based row 1, UTF-8 byte columns 45..46.
        assert_eq!(old[0].1, [1, 45, 1, 46]);
        assert_eq!(new[0].1, [1, 45, 1, 46]);
        assert_ne!(old[0].2, new[0].2);
        assert_ne!(proc_hash(PLUS, "combine"), proc_hash(MINUS, "combine"));
    }

    #[test]
    fn operator_issue_pair_is_a_real_change() {
        // Exact pair from odin-parser#4.
        let old = "package main\nf :: proc(a,b:int) -> int { return a + b }\n";
        let new = "package main\nf :: proc(a,b:int) -> int { return a - b }\n";
        assert_eq!(operators(old)[0].0, "+");
        assert_eq!(operators(new)[0].0, "-");
        assert_ne!(proc_hash(old, "f"), proc_hash(new, "f"));
    }

    #[test]
    fn operator_leaf_sits_under_its_expression() {
        let v = tree(PLUS);
        let mut exprs = Vec::new();
        collect(&v, "binary_expression", &mut exprs);
        assert_eq!(exprs.len(), 1);
        let kids = exprs[0]["children"].as_array().unwrap();
        assert!(kids.iter().any(|c| c["node_type"] == OPERATOR_NODE_TYPE));
    }

    #[test]
    fn compound_unary_range_and_word_operators_are_kept() {
        let src = "package probe\nf :: proc(a, b: int, m: Maybe(int)) -> int {\n\tx := a\n\tx += -b\n\tfor i in 0..<b { x |= i }\n\treturn m.? or_else x\n}\n";
        let labels: Vec<String> = operators(src).into_iter().map(|o| o.0).collect();
        for op in ["+=", "-", "..<", "|=", "or_else"] {
            assert!(
                labels.iter().any(|l| l == op),
                "{op} missing from {labels:?}"
            );
        }
        // Declaration syntax is not an operator.
        for not_op in ["::", ":=", "->", ":"] {
            assert!(
                !labels.iter().any(|l| l == not_op),
                "{not_op} in {labels:?}"
            );
        }
        let closed = src.replace("0..<b", "0..=b");
        assert!(operators(&closed).iter().any(|o| o.0 == "..="));
        assert_ne!(proc_hash(src, "f"), proc_hash(&closed, "f"));
    }

    #[test]
    fn identity_output_is_deterministic() {
        assert_eq!(process_impl(PLUS), process_impl(PLUS));
    }

    #[test]
    fn comments_and_layout_do_not_change_hashes() {
        let commented = "package probe\ncombine :: proc(a, b: int) -> int { /* sum */ return a + b // done\n}\n";
        let laid_out =
            "package probe\n\ncombine :: proc(a, b: int) -> int {\n\treturn a  +  b\n}\n";
        let base = proc_hash(PLUS, "combine");
        assert_eq!(proc_hash(commented, "combine"), base);
        assert_eq!(proc_hash(laid_out, "combine"), base);
        let v = tree(commented);
        let mut comments = Vec::new();
        collect(&v, "comment", &mut comments);
        collect(&v, "block_comment", &mut comments);
        assert!(comments.is_empty());
    }

    #[test]
    fn long_literal_tail_edit_changes_hash() {
        // The old converter capped leaf text at 4096 chars, hiding tail edits.
        let body = "x".repeat(5000);
        let old = format!("package probe\ns :: proc() -> string {{ return \"{body}a\" }}\n");
        let new = format!("package probe\ns :: proc() -> string {{ return \"{body}b\" }}\n");
        assert_ne!(proc_hash(&old, "s"), proc_hash(&new, "s"));
    }

    #[test]
    fn literal_value_edit_still_surfaces() {
        let old = "package probe\nanswer :: proc() -> int { return 1 }\n";
        let new = "package probe\nanswer :: proc() -> int { return 2 }\n";
        assert!(find_label(&tree(old), "1").is_some());
        assert!(find_label(&tree(new), "2").is_some());
    }

    #[test]
    fn multibyte_source_keeps_byte_columns() {
        let src = "package probe\ncombine :: proc(a, b: int) -> int { s := \"\u{e9}\u{e9}\"; return a + b }\n";
        let ops = operators(src);
        let line = src.lines().nth(1).unwrap();
        let col = line.find(" + ").unwrap() as u64 + 1;
        assert!(
            ops.iter()
                .any(|o| o.0 == "+" && o.1 == [1, col, 1, col + 1]),
            "{ops:?}"
        );
    }

    #[test]
    fn optional_statement_separators_do_not_change_hashes() {
        // Exact pair from odin-parser#6: an explicit `;` terminator is optional.
        let old = "main :: proc() {\n\tx := 1\n}";
        let new = "main :: proc() {\n\tx := 1;\n}";
        assert_eq!(proc_hash(old, "main"), proc_hash(new, "main"));
        assert_eq!(tree(old)["structural_hash"], tree(new)["structural_hash"]);
        let one_line = "package p\nmain :: proc() { x := 1; y := 2; }\n";
        let lines = "package p\nmain :: proc() {\n\tx := 1\n\ty := 2\n}\n";
        assert_eq!(proc_hash(one_line, "main"), proc_hash(lines, "main"));
        assert_eq!(
            tree("package p;\nimport \"core:fmt\";\n")["structural_hash"],
            tree("package p\nimport \"core:fmt\"\n")["structural_hash"]
        );
        // Operators stay meaningful.
        assert_ne!(
            proc_hash("main :: proc() {\n\tx := 1 + 2;\n}", "main"),
            proc_hash("main :: proc() {\n\tx := 1 - 2;\n}", "main")
        );
        // `for` clause separators are syntax, not terminators: they stay in the CST.
        fn semicolons(n: &CstNode, parent: &str, out: &mut Vec<String>) {
            if n.node_type == ";" {
                out.push(parent.to_string());
            }
            for c in &n.children {
                semicolons(c, &n.node_type, out);
            }
        }
        let cst =
            parse_source("package p;\nf :: proc() { for i := 0; i < 3; i += 1 { }; }\n").unwrap();
        let mut parents = Vec::new();
        semicolons(&cst, "", &mut parents);
        assert_eq!(parents, ["for_statement", "for_statement"]);
    }
}
