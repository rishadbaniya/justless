use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use tree_sitter::{Language, Node, Parser, Query, QueryCursor, StreamingIterator};

use crate::rust_syntax::Kind;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum Grammar {
    JavaScript,
    TypeScript,
    Tsx,
    Python,
    Go,
    Java,
    C,
    Cpp,
    CSharp,
    Ruby,
    Php,
    Bash,
    Sql,
}

const SQL_TAGS: &str = "
(create_table (object_reference) @name) @definition.table
(create_view (object_reference) @name) @definition.view
(relation (object_reference) @name) @reference.table
";

impl Grammar {
    pub fn for_file(file: &str) -> Option<Self> {
        let path = Path::new(file);
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match extension.as_str() {
            "js" | "jsx" | "mjs" | "cjs" => Self::JavaScript,
            "ts" | "mts" | "cts" => Self::TypeScript,
            "tsx" => Self::Tsx,
            "py" | "pyi" => Self::Python,
            "go" => Self::Go,
            "java" => Self::Java,
            "c" | "h" => Self::C,
            "cc" | "cpp" | "cxx" | "hpp" | "hxx" => Self::Cpp,
            "cs" => Self::CSharp,
            "rb" | "rake" => Self::Ruby,
            "php" | "phtml" => Self::Php,
            "sh" | "bash" => Self::Bash,
            "sql" => Self::Sql,
            _ => return None,
        })
    }

    pub fn family(self) -> Self {
        match self {
            Self::TypeScript | Self::Tsx => Self::JavaScript,
            Self::Cpp => Self::C,
            other => other,
        }
    }

    fn parser(self) -> (Language, String) {
        let (language, tags) = match self {
            Self::JavaScript => (
                tree_sitter_javascript::LANGUAGE,
                tree_sitter_javascript::TAGS_QUERY,
            ),
            Self::TypeScript => (
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
                tree_sitter_typescript::TAGS_QUERY,
            ),
            Self::Tsx => (
                tree_sitter_typescript::LANGUAGE_TSX,
                tree_sitter_typescript::TAGS_QUERY,
            ),
            Self::Python => (tree_sitter_python::LANGUAGE, tree_sitter_python::TAGS_QUERY),
            Self::Go => (tree_sitter_go::LANGUAGE, tree_sitter_go::TAGS_QUERY),
            Self::Java => (tree_sitter_java::LANGUAGE, tree_sitter_java::TAGS_QUERY),
            Self::C => (tree_sitter_c::LANGUAGE, tree_sitter_c::TAGS_QUERY),
            Self::Cpp => (tree_sitter_cpp::LANGUAGE, tree_sitter_cpp::TAGS_QUERY),
            Self::CSharp => (
                tree_sitter_c_sharp::LANGUAGE,
                tree_sitter_c_sharp::TAGS_QUERY,
            ),
            Self::Ruby => (tree_sitter_ruby::LANGUAGE, tree_sitter_ruby::TAGS_QUERY),
            Self::Php => (tree_sitter_php::LANGUAGE_PHP, tree_sitter_php::TAGS_QUERY),
            Self::Bash => (
                tree_sitter_bash::LANGUAGE,
                "
                (function_definition name: (word) @name) @definition.function
                (command name: (command_name (word) @name)) @reference.call",
            ),
            Self::Sql => (tree_sitter_sequel::LANGUAGE, SQL_TAGS),
        };
        let mut tags = tags.to_owned();
        if matches!(self, Self::TypeScript | Self::Tsx) {
            tags.push_str(tree_sitter_javascript::TAGS_QUERY);
            tags.push_str(
                "\n(type_alias_declaration name: (_) @name) @definition.type
                (enum_declaration name: (_) @name) @definition.enum
                (class_declaration name: (type_identifier) @name) @definition.class\n",
            );
        }
        if matches!(self, Self::C | Self::Cpp) {
            tags.push_str("\n(call_expression function: (identifier) @name) @reference.call\n");
        }
        (language.into(), tags)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Definition {
    #[serde(skip)]
    pub bytes: std::ops::Range<usize>,
    #[serde(skip)]
    pub header_end: usize,
    pub name: String,
    pub kind: String,
    pub path: String,
    pub start: usize,
    pub end: usize,
    pub declaration: usize,
    pub test: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Reference {
    pub name: String,
    pub from: Option<usize>,
    pub line: usize,
    pub kind: Kind,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Import {
    pub path: String,
    pub line: usize,
    pub bindings: BTreeMap<String, String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Facts {
    #[serde(skip)]
    pub import_ranges: Vec<std::ops::Range<usize>>,
    pub grammar: Grammar,
    pub lines: usize,
    pub definitions: Vec<Definition>,
    pub references: Vec<Reference>,
    pub imports: Vec<Import>,
    pub routes: Vec<(String, usize)>,
}

struct RawTag<'a> {
    name: &'a str,
    node: Node<'a>,
    name_node: Node<'a>,
    label: String,
}

struct Compiled {
    parser: Parser,
    query: Query,
}

impl Compiled {
    fn new(grammar: Grammar) -> anyhow::Result<Self> {
        let (language, tags) = grammar.parser();
        let mut parser = Parser::new();
        parser.set_language(&language)?;
        Ok(Self {
            parser,
            query: Query::new(&language, &tags)?,
        })
    }
}

#[derive(Default)]
pub struct Parsers(BTreeMap<Grammar, Compiled>);

impl Parsers {
    pub fn parse(&mut self, grammar: Grammar, source: &str) -> anyhow::Result<Facts> {
        if let std::collections::btree_map::Entry::Vacant(entry) = self.0.entry(grammar) {
            entry.insert(Compiled::new(grammar)?);
        }
        let Compiled { parser, query } = self.0.get_mut(&grammar).unwrap();
        let tree = parser
            .parse(source, None)
            .ok_or_else(|| anyhow::anyhow!("parse cancelled"))?;
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(query, tree.root_node(), source.as_bytes());
        let mut definitions = Vec::new();
        let mut references = Vec::new();
        while let Some(matched) = matches.next() {
            let captures = query.capture_names();
            let Some(name) = matched
                .captures
                .iter()
                .find(|cap| captures[cap.index as usize] == "name")
            else {
                continue;
            };
            let name_text = text(name.node, source);
            if name_text.is_empty() {
                continue;
            }
            for capture in matched.captures {
                let label = captures[capture.index as usize];
                let tag = RawTag {
                    name: name_text,
                    node: capture.node,
                    name_node: name.node,
                    label: label.to_owned(),
                };
                if label.starts_with("definition.") {
                    definitions.push(tag);
                } else if label.starts_with("reference.") {
                    references.push(tag);
                }
            }
        }
        definitions.sort_by_key(|tag| {
            (
                tag.node.start_byte(),
                std::cmp::Reverse(tag.node.end_byte()),
                tag.name_node.start_byte(),
            )
        });
        definitions.dedup_by_key(|tag| tag.name_node.byte_range());
        let mut facts = Facts {
            import_ranges: Vec::new(),
            grammar,
            lines: source.lines().count().max(1),
            definitions: Vec::new(),
            references: Vec::new(),
            imports: Vec::new(),
            routes: Vec::new(),
        };
        for (id, definition) in definitions.iter().enumerate() {
            let RawTag {
                node,
                name_node,
                name,
                label,
            } = definition;
            let kind = label.strip_prefix("definition.").unwrap();
            let name = if grammar == Grammar::Sql {
                sql_name(name)
            } else {
                name.to_string()
            };
            let path = parent_symbol(&definitions[..id], *node)
                .map(|parent| format!("{}::{name}", facts.definitions[parent].path))
                .unwrap_or_else(|| name.clone());
            let test = (grammar == Grammar::Python && name.starts_with("test_"))
                || (grammar == Grammar::Go
                    && name
                        .strip_prefix("Test")
                        .is_some_and(|suffix| suffix.starts_with(char::is_uppercase)));
            let extent = declaration_extent(*node, *name_node);
            let body = node.child_by_field_name("body").or_else(|| {
                node.child_by_field_name("value")?
                    .child_by_field_name("body")
            });
            let header_end = if grammar == Grammar::Python {
                let mut cursor = node.walk();
                node.children(&mut cursor)
                    .find(|child| child.kind() == ":")
                    .map(|colon| colon.end_byte())
            } else {
                None
            };
            facts.definitions.push(Definition {
                bytes: extent.byte_range(),
                header_end: header_end.unwrap_or_else(|| {
                    body.or_else(|| extent.child_by_field_name("body"))
                        .map_or(extent.end_byte(), |body| body.start_byte())
                }),
                name,
                path,
                kind: kind.to_owned(),
                start: node.start_position().row + 1,
                end: last_line(*node),
                declaration: name_node.start_position().row + 1,
                test,
            });
        }
        let mut seen = BTreeSet::new();
        for tag in references {
            let kind = match tag.label.as_str() {
                "reference.call" | "reference.send" => Kind::Call,
                "reference.table" => Kind::Data,
                _ => Kind::Reference,
            };
            let name = if kind == Kind::Data {
                sql_name(tag.name)
            } else {
                tag.name.to_owned()
            };
            let line = tag.name_node.start_position().row + 1;
            let from = parent_symbol(&definitions, tag.node);
            if seen.insert((kind, line, from, name.clone())) {
                facts.references.push(Reference {
                    name,
                    from,
                    line,
                    kind,
                });
            }
        }
        let mut embedded_queries = Vec::new();
        walk(tree.root_node(), |node| {
            if matches!(
                node.kind(),
                "import_statement"
                    | "import_from_statement"
                    | "import_declaration"
                    | "preproc_include"
                    | "using_directive"
                    | "namespace_use_declaration"
            ) {
                facts.import_ranges.push(node.byte_range());
            }
            if let Some(import) = module_import(grammar, node, source) {
                facts.imports.push(import);
            }
            if let Some((route, handler, argument)) = route_use(grammar, node, source) {
                if let Some(handler) = handler {
                    if let Some(id) = parent_symbol(&definitions, handler) {
                        facts.routes.push((route.to_owned(), id));
                    }
                } else {
                    facts.references.push(Reference {
                        name: route.to_owned(),
                        from: parent_symbol(&definitions, node),
                        line: argument.start_position().row + 1,
                        kind: Kind::Route,
                    });
                }
            }
            if grammar != Grammar::Sql
                && let Some(sql) = embedded_sql(node, source)
            {
                embedded_queries.push((
                    sql,
                    parent_symbol(&definitions, node),
                    node.start_position().row,
                ));
            }
        });
        for (sql, from, row) in embedded_queries {
            // Embedded statements use the same table extractor as SQL files.
            if let Ok(embedded) = self.parse(Grammar::Sql, sql) {
                facts.references.extend(
                    embedded
                        .references
                        .into_iter()
                        .filter(|reference| reference.kind == Kind::Data)
                        .map(|reference| Reference {
                            from,
                            line: row + reference.line,
                            ..reference
                        }),
                );
            }
        }
        Ok(facts)
    }
}

// Some tag queries capture a declarator rather than the declaration that owns
// its type, modifiers and body. Never widen a multi-declarator statement.
fn declaration_extent<'a>(mut node: Node<'a>, name: Node<'a>) -> Node<'a> {
    if node.kind() == "property_declaration" {
        let mut cursor = node.walk();
        let properties: Vec<_> = node
            .named_children(&mut cursor)
            .filter(|c| c.kind() == "property_element")
            .collect();
        if properties.len() > 1
            && let Some(property) = properties
                .into_iter()
                .find(|p| p.start_byte() <= name.start_byte() && name.end_byte() <= p.end_byte())
        {
            return property;
        }
    }
    while let Some(parent) = node.parent() {
        let widen = match parent.kind() {
            "export_statement" | "decorated_definition" => {
                parent.named_child_count() == 1
                    || parent.child_by_field_name("definition") == Some(node)
                    || parent.child_by_field_name("declaration") == Some(node)
            }
            "function_definition" | "pointer_declarator" | "reference_declarator" => {
                parent.child_by_field_name("declarator") == Some(node)
            }
            "declaration" | "field_declaration" => {
                let mut cursor = parent.walk();
                let mut declarators = parent.children_by_field_name("declarator", &mut cursor);
                declarators.next() == Some(node) && declarators.next().is_none()
            }
            "lexical_declaration" | "variable_declaration" => {
                node.kind() == "variable_declarator" && parent.named_child_count() == 1
            }
            _ => false,
        };
        if !widen {
            break;
        }
        node = parent;
    }
    node
}

fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    source.get(node.byte_range()).unwrap_or("")
}

fn last_line(node: Node<'_>) -> usize {
    let end = node.end_position();
    (end.row + usize::from(end.column != 0)).max(node.start_position().row + 1)
}

fn parent_symbol(tags: &[RawTag<'_>], node: Node<'_>) -> Option<usize> {
    let end = tags.partition_point(|tag| tag.node.start_byte() <= node.start_byte());
    (0..end)
        .rev()
        .find(|&id| tags[id].node.end_byte() >= node.end_byte())
}

fn walk(root: Node<'_>, mut visit: impl FnMut(Node<'_>)) {
    let mut cursor = root.walk();
    loop {
        visit(cursor.node());
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return;
            }
        }
    }
}

fn literal(value: &str) -> Option<&str> {
    let value = value.trim_start_matches(['r', 'R', 'u', 'U', 'b', 'B']);
    for quote in ["\"\"\"", "'''", "\"", "'", "\u{60}"] {
        if let Some(body) = value
            .strip_prefix(quote)
            .and_then(|body| body.strip_suffix(quote))
        {
            return Some(body);
        }
    }
    None
}

fn sql_name(value: &str) -> String {
    value
        .split('.')
        .map(|part| {
            part.trim()
                .trim_matches(['"', '\u{60}'])
                .to_ascii_lowercase()
        })
        .collect::<Vec<_>>()
        .join(".")
}

fn embedded_sql<'a>(node: Node<'_>, source: &'a str) -> Option<&'a str> {
    if !matches!(
        node.kind(),
        "string"
            | "template_string"
            | "string_literal"
            | "interpreted_string_literal"
            | "raw_string_literal"
    ) || node.has_error()
    {
        return None;
    }
    let body = literal(text(node, source))?;
    let first = body.split_whitespace().next()?.to_ascii_uppercase();
    matches!(
        first.as_str(),
        "SELECT" | "WITH" | "INSERT" | "UPDATE" | "DELETE"
    )
    .then_some(body)
}

fn route_use<'a, 'tree>(
    grammar: Grammar,
    node: Node<'tree>,
    source: &'a str,
) -> Option<(&'a str, Option<Node<'tree>>, Node<'tree>)> {
    let python = grammar == Grammar::Python && node.kind() == "decorator";
    let javascript = grammar.family() == Grammar::JavaScript && node.kind() == "call_expression";
    if !python && !javascript {
        return None;
    }
    let call = if python { node.named_child(0)? } else { node };
    let callee = text(call.child_by_field_name("function")?, source);
    let argument = call.child_by_field_name("arguments")?.named_child(0)?;
    let route = literal(text(argument, source))?;
    if !route.starts_with('/')
        || route.starts_with("//")
        || route.contains(['\n', '\r', '?', '#', '\\', '$'])
    {
        return None;
    }
    if python {
        let (_, method) = callee.rsplit_once('.')?;
        if !matches!(
            method,
            "get" | "post" | "put" | "patch" | "delete" | "route" | "api_route"
        ) {
            return None;
        }
        let handler = node.parent()?.child_by_field_name("definition")?;
        Some((route, Some(handler), argument))
    } else if callee == "fetch" || callee.strip_prefix("axios.").is_some() {
        Some((route, None, argument))
    } else {
        None
    }
}

fn module_import(grammar: Grammar, node: Node<'_>, source: &str) -> Option<Import> {
    let mut bindings = BTreeMap::new();
    let path = if grammar.family() == Grammar::JavaScript && node.kind() == "import_statement" {
        let path = literal(text(node.child_by_field_name("source")?, source))?;
        walk(node, |child| {
            if child.kind() == "import_specifier"
                && let Some(name) = child.child_by_field_name("name")
            {
                let alias = child.child_by_field_name("alias").unwrap_or(name);
                bindings.insert(
                    text(alias, source).to_owned(),
                    text(name, source).to_owned(),
                );
            }
        });
        path
    } else if grammar == Grammar::Python && node.kind() == "import_from_statement" {
        let module = node.child_by_field_name("module_name")?;
        let mut cursor = node.walk();
        for child in node.children_by_field_name("name", &mut cursor) {
            let name = child.child_by_field_name("name").unwrap_or(child);
            let alias = child.child_by_field_name("alias").unwrap_or(name);
            bindings.insert(
                text(alias, source).to_owned(),
                text(name, source).to_owned(),
            );
        }
        text(module, source)
    } else if matches!(grammar, Grammar::C | Grammar::Cpp) && node.kind() == "preproc_include" {
        literal(text(node.child_by_field_name("path")?, source))?
    } else {
        return None;
    };
    Some(Import {
        path: path.to_owned(),
        bindings,
        line: node.start_position().row + 1,
    })
}
