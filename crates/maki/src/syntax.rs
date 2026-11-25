use std::path::Path;
use tree_sitter::{Language, Node, Parser, Tree};

pub(crate) struct Grammar {
    pub name: &'static str,
    language: Language,
}

impl Grammar {
    pub fn for_path(path: &Path) -> Option<Self> {
        let (name, language) = match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "rs" => ("rust", tree_sitter_rust::LANGUAGE),
            "ts" | "mts" | "cts" => ("typescript", tree_sitter_typescript::LANGUAGE_TYPESCRIPT),
            "tsx" => ("tsx", tree_sitter_typescript::LANGUAGE_TSX),
            "js" | "jsx" | "mjs" | "cjs" => ("javascript", tree_sitter_javascript::LANGUAGE),
            "py" | "pyi" => ("python", tree_sitter_python::LANGUAGE),
            "go" => ("go", tree_sitter_go::LANGUAGE),
            "sh" | "bash" => ("bash", tree_sitter_bash::LANGUAGE),
            "json" => ("json", tree_sitter_json::LANGUAGE),
            _ => return None,
        };
        Some(Self {
            name,
            language: language.into(),
        })
    }

    pub fn parse(&self, source: &str) -> Option<Tree> {
        let mut parser = Parser::new();
        parser.set_language(&self.language).ok()?;
        parser.parse(source, None)
    }
}

pub(crate) struct Symbol<'tree, 'source> {
    pub node: Node<'tree>,
    pub kind: &'static str,
    pub name: &'source str,
}

impl<'tree, 'source> Symbol<'tree, 'source> {
    pub fn from_node(node: Node<'tree>, source: &'source str) -> Option<Self> {
        let kind = match node.kind() {
            "function_item"
            | "function_signature_item"
            | "function_declaration"
            | "generator_function_declaration"
            | "function_definition"
            | "function_signature" => "fn",
            "method_definition"
            | "method_declaration"
            | "method_signature"
            | "abstract_method_signature" => "method",
            "struct_item" => "struct",
            "enum_item" | "enum_declaration" => "enum",
            "trait_item" => "trait",
            "impl_item" => "impl",
            "class_declaration" | "abstract_class_declaration" | "class_definition" => "class",
            "interface_declaration" => "interface",
            "type_item" | "type_alias_declaration" | "type_spec" | "type_alias" => "type",
            "mod_item" => "mod",
            "export_statement" if node.child_by_field_name("declaration").is_none() => "export",
            "variable_declarator" if is_arrow_or_function(node) => "fn",
            "variable_declarator"
                if node
                    .parent()
                    .and_then(|parent| parent.parent())
                    .is_some_and(|parent| parent.kind() == "export_statement") =>
            {
                "export"
            }
            "pair" if *node.language() == tree_sitter_json::LANGUAGE.into() => "key",
            _ => return None,
        };
        let (kind, name_node) = match kind {
            "impl" => (kind, node.child_by_field_name("type")),
            "type" if node.kind() == "type_spec" => {
                let kind = match node.child_by_field_name("type").map(|ty| ty.kind()) {
                    Some("struct_type") => "struct",
                    Some("interface_type") => "interface",
                    _ => kind,
                };
                (kind, node.child_by_field_name("name"))
            }
            "export" if node.kind() == "export_statement" => (
                kind,
                node.child_by_field_name("source")
                    .or_else(|| node.child_by_field_name("value"))
                    .or_else(|| node.named_child(0)),
            ),
            "key" => (kind, node.child_by_field_name("key")),
            _ => (kind, node.child_by_field_name("name")),
        };
        let name = source.get(name_node?.byte_range())?;
        let name = if matches!(kind, "key" | "export") {
            name.strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .unwrap_or(name)
        } else {
            name
        };
        if name.is_empty() {
            return None;
        }
        Some(Self { node, kind, name })
    }

    pub fn signature(&self, source: &str) -> String {
        let node = self.node;
        let mut value = node.child_by_field_name("value");
        if value.is_some_and(|value| {
            matches!(
                value.kind(),
                "arrow_function" | "function_expression" | "generator_function"
            )
        }) {
            value = value.and_then(|value| value.child_by_field_name("body"));
        }
        let end = node
            .child_by_field_name("body")
            .or_else(|| {
                node.child_by_field_name("type")
                    .filter(|ty| matches!(ty.kind(), "struct_type" | "interface_type"))
            })
            .or(value)
            .map_or(node.end_byte(), |body| body.start_byte());
        let mut signature = String::new();
        let declaration =
            if node.kind() == "variable_declarator" && node.prev_named_sibling().is_none() {
                node.parent().unwrap_or(node)
            } else {
                node
            };
        let start = declaration
            .parent()
            .filter(|parent| parent.kind() == "export_statement")
            .unwrap_or(declaration)
            .start_byte();
        for word in source[start..end].split_whitespace() {
            if !signature.is_empty() {
                signature.push(' ');
            }
            let available = 240_usize.saturating_sub(signature.len());
            if word.len() > available {
                signature.push_str(crate::utf8_prefix(word, available));
                signature.push('…');
                break;
            }
            signature.push_str(word);
        }
        signature
    }

    pub fn lines(&self) -> (usize, usize) {
        let start = self.node.start_position().row + 1;
        let end = self.node.end_position();
        (start, (end.row + usize::from(end.column > 0)).max(start))
    }
}

fn is_arrow_or_function(node: Node<'_>) -> bool {
    node.child_by_field_name("value").is_some_and(|value| {
        matches!(
            value.kind(),
            "arrow_function" | "function_expression" | "generator_function"
        )
    })
}

pub(crate) fn symbols<'tree, 'source>(
    tree: &'tree Tree,
    source: &'source str,
) -> impl Iterator<Item = Symbol<'tree, 'source>> {
    let mut cursor = tree.walk();
    let mut finished = false;
    std::iter::from_fn(move || {
        if finished {
            return None;
        }
        let node = cursor.node();
        if !cursor.goto_first_child() {
            while !cursor.goto_next_sibling() {
                if !cursor.goto_parent() {
                    finished = true;
                    break;
                }
            }
        }
        Some(node)
    })
    .filter_map(|node| Symbol::from_node(node, source))
}

pub(crate) fn enclosing_symbol<'tree, 'source>(
    tree: &'tree Tree,
    source: &'source str,
    byte: usize,
) -> Option<Symbol<'tree, 'source>> {
    let mut node = tree.root_node().descendant_for_byte_range(byte, byte)?;
    loop {
        if let Some(symbol) = Symbol::from_node(node, source) {
            return Some(symbol);
        }
        node = node.parent()?;
    }
}
