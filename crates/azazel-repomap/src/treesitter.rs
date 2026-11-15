//! The extractor that reads source with tree-sitter grammars (feature `treesitter`).

use std::collections::HashMap;

use tree_sitter::{Language, Node, Parser};

use crate::extract::{Def, Lang, MAX_SIGNATURE, Tags};

/// Deepest nesting of the syntax tree that is followed; anything below is machine-made.
const MAX_DEPTH: usize = 160;

fn language(lang: Lang) -> Language {
    match lang {
        Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
        Lang::Python => tree_sitter_python::LANGUAGE.into(),
        Lang::Go => tree_sitter_go::LANGUAGE.into(),
        Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
    }
}

enum Shape {
    /// A function or method: what is defined inside its body is not recorded.
    Function,
    /// A type, constant or other definition without definitions of its own.
    Leaf,
    /// Holds definitions: a trait, class, module, interface.
    Container,
    /// Holds definitions but has no name of its own: `impl Foo`.
    Group,
}

fn shape(lang: Lang, node: Node) -> Option<Shape> {
    let kind = node.kind();
    match lang {
        Lang::Rust => match kind {
            "function_item" | "function_signature_item" => Some(Shape::Function),
            "struct_item" | "enum_item" | "union_item" | "type_item" | "const_item" | "static_item"
            | "macro_definition" => Some(Shape::Leaf),
            "trait_item" | "mod_item" => Some(Shape::Container),
            "impl_item" => Some(Shape::Group),
            _ => None,
        },
        Lang::Python => match kind {
            "function_definition" => Some(Shape::Function),
            "class_definition" => Some(Shape::Container),
            _ => None,
        },
        Lang::Go => match kind {
            "function_declaration" | "method_declaration" => Some(Shape::Function),
            "type_spec" => Some(Shape::Leaf),
            _ => None,
        },
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => match kind {
            "function_declaration"
            | "generator_function_declaration"
            | "method_definition"
            | "function_signature"
            | "method_signature"
            | "abstract_method_signature" => Some(Shape::Function),
            "class_declaration" | "abstract_class_declaration" | "interface_declaration" => {
                Some(Shape::Container)
            }
            "enum_declaration" | "type_alias_declaration" => Some(Shape::Leaf),
            "variable_declarator" => {
                let value = node.child_by_field_name("value")?;
                matches!(
                    value.kind(),
                    "arrow_function" | "function_expression" | "function"
                )
                .then_some(Shape::Function)
            }
            _ => None,
        },
    }
}

struct Walk<'a> {
    lang: Lang,
    source: &'a str,
    defs: Vec<Def>,
    refs: HashMap<&'a str, u32>,
}

fn is_identifier(kind: &str) -> bool {
    matches!(
        kind,
        "identifier" | "type_identifier" | "field_identifier" | "property_identifier"
    )
}

impl<'a> Walk<'a> {
    fn text(&self, node: Node) -> &'a str {
        self.source.get(node.start_byte()..node.end_byte()).unwrap_or("")
    }

    fn visit(
        &mut self,
        node: Node<'a>,
        parent: Option<u32>,
        depth: u32,
        inside_function: bool,
        level: usize,
    ) {
        if level > MAX_DEPTH {
            return;
        }
        if node.child_count() == 0 {
            if is_identifier(node.kind()) {
                let name = self.text(node);
                if name.len() >= 2 {
                    *self.refs.entry(name).or_default() += 1;
                }
            }
            return;
        }
        let mut parent = parent;
        let mut depth = depth;
        let mut inside_function = inside_function;
        if !inside_function && let Some(shape) = shape(self.lang, node) {
            if self.is_test_module(node) {
                return;
            }
            let name = match shape {
                Shape::Group => Some(""),
                _ => node.child_by_field_name("name").map(|n| self.text(n)),
            };
            if let Some(name) = name {
                let index = self.defs.len() as u32;
                let def = self.define(node, name, parent, depth);
                self.defs.push(def);
                match shape {
                    Shape::Function => inside_function = true,
                    Shape::Leaf => {}
                    Shape::Container | Shape::Group => {
                        parent = Some(index);
                        depth += 1;
                    }
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.visit(child, parent, depth, inside_function, level + 1);
        }
    }

    /// `mod tests { … }`: its definitions and what it uses say nothing about the project.
    fn is_test_module(&self, node: Node) -> bool {
        self.lang == Lang::Rust
            && node.kind() == "mod_item"
            && node.child_by_field_name("body").is_some()
            && node
                .child_by_field_name("name")
                .is_some_and(|n| self.text(n) == "tests")
    }

    fn define(&self, node: Node<'a>, name: &str, parent: Option<u32>, depth: u32) -> Def {
        // `export function f()`, `const f = () => …`: the signature starts where the statement does.
        let mut start = node;
        if let Some(up) = node.parent()
            && matches!(
                up.kind(),
                "export_statement" | "lexical_declaration" | "variable_declaration"
            )
            && up.named_child_count() <= 1 + usize::from(up.kind() == "export_statement")
        {
            start = up;
            if let Some(outer) = up.parent()
                && outer.kind() == "export_statement"
            {
                start = outer;
            }
        }
        let body = node
            .child_by_field_name("body")
            .or_else(|| {
                node.child_by_field_name("value")
                    .and_then(|v| v.child_by_field_name("body"))
            })
            .or_else(|| node.child_by_field_name("value"));
        let mut end = node.end_byte();
        if let Some(body) = body
            && body.start_byte() > node.start_byte()
        {
            end = body.start_byte();
        }
        let raw = self.source.get(start.start_byte()..end).unwrap_or("");
        let mut raw = raw.trim_end().trim_end_matches(['{', '=', ' ', '\t']);
        if self.lang == Lang::Go && node.kind() == "type_spec" {
            raw = raw
                .lines()
                .next()
                .unwrap_or(raw)
                .trim_end()
                .trim_end_matches('{')
                .trim_end();
        }
        let rows = raw.lines().count().max(1) as u32;
        let mut sig = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        if self.lang == Lang::Go && node.kind() == "type_spec" {
            sig = format!("type {sig}");
        }
        if sig.chars().count() > MAX_SIGNATURE {
            sig = sig.chars().take(MAX_SIGNATURE - 1).collect::<String>() + "…";
        }
        Def {
            name: name.to_string(),
            line: start.start_position().row as u32 + 1,
            rows,
            parent,
            depth,
            sig,
        }
    }
}

/// The definitions of `source` and the identifiers it uses; `None` when it cannot be parsed.
pub fn extract(lang: Lang, source: &str) -> Option<Tags> {
    let mut parser = Parser::new();
    parser.set_language(&language(lang)).ok()?;
    let tree = parser.parse(source, None)?;
    let mut walk = Walk {
        lang,
        source,
        defs: Vec::new(),
        refs: HashMap::new(),
    };
    walk.visit(tree.root_node(), None, 0, false, 0);
    let mut refs: Vec<(String, u32)> = walk.refs.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    refs.sort();
    Some(Tags {
        defs: walk.defs,
        refs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(tags: &Tags) -> Vec<&str> {
        tags.defs.iter().map(|d| d.name.as_str()).collect()
    }

    #[test]
    fn rust_definitions_carry_their_signature_and_nest_under_impls() {
        let tags = extract(
            Lang::Rust,
            "pub struct Point {\n    x: i32,\n}\n\nimpl Point {\n    pub fn new(\n        x: i32,\n    ) -> Point {\n        helper(x)\n    }\n}\n\nfn helper(x: i32) -> Point { Point { x } }\n",
        )
        .unwrap();
        assert_eq!(names(&tags), ["Point", "", "new", "helper"]);
        assert_eq!(tags.defs[0].sig, "pub struct Point");
        assert_eq!(tags.defs[2].sig, "pub fn new( x: i32, ) -> Point");
        assert_eq!((tags.defs[2].parent, tags.defs[2].depth), (Some(1), 1));
        assert!(tags.refs.contains(&("helper".to_string(), 2)));
    }

    #[test]
    fn python_class_methods_are_definitions_and_nested_functions_are_not() {
        let tags = extract(
            Lang::Python,
            "class Store(Base):\n    def get(self, key):\n        def inner():\n            pass\n        return inner\n",
        )
        .unwrap();
        assert_eq!(names(&tags), ["Store", "get"]);
        assert_eq!(tags.defs[1].sig, "def get(self, key):");
    }

    #[test]
    fn typescript_arrow_functions_and_interfaces_are_definitions() {
        let tags = extract(
            Lang::TypeScript,
            "export const load = async (id: string): Promise<Item> => { return fetchItem(id); };\ninterface Item { id: string }\n",
        )
        .unwrap();
        assert_eq!(names(&tags), ["load", "Item"]);
        assert!(
            tags.defs[0]
                .sig
                .starts_with("export const load = async (id: string)")
        );
    }

    #[test]
    fn go_functions_and_types() {
        let tags = extract(
            Lang::Go,
            "package a\n\ntype Server struct {\n\tport int\n}\n\nfunc (s *Server) Start() error {\n\treturn nil\n}\n",
        )
        .unwrap();
        assert_eq!(names(&tags), ["Server", "Start"]);
        assert_eq!(tags.defs[0].sig, "type Server struct");
    }

    #[test]
    fn test_modules_are_left_out() {
        let tags = extract(
            Lang::Rust,
            "fn real() {}\n#[cfg(test)]\nmod tests {\n    fn only_in_tests() { real(); }\n}\n",
        )
        .unwrap();
        assert_eq!(names(&tags), ["real"]);
        assert!(tags.refs.iter().all(|(name, _)| name != "only_in_tests"));
    }
}
