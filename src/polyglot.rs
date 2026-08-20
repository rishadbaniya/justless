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
    Kotlin,
    Swift,
    Scala,
    Dart,
    Hcl,
    Lua,
    Zig,
}

const SQL_TAGS: &str = "
(create_table (object_reference) @name) @definition.table
(create_view (object_reference) @name) @definition.view
(relation (object_reference) @name) @reference.table
";

// Member patterns are anchored to a type body or the file so that locals
// inside function bodies never become outline rows or resolution targets.
const KOTLIN_TAGS: &str = r#"
(class_declaration "class" name: (identifier) @name) @definition.class
(class_declaration "interface" name: (identifier) @name) @definition.interface
(object_declaration name: (identifier) @name) @definition.object
(type_alias type: (identifier) @name) @definition.type
(source_file (function_declaration name: (identifier) @name) @definition.function)
(class_body (function_declaration name: (identifier) @name) @definition.method)
(enum_class_body (function_declaration name: (identifier) @name) @definition.method)
(source_file
  (property_declaration (variable_declaration (identifier) @name)) @definition.property)
(class_body
  (property_declaration (variable_declaration (identifier) @name)) @definition.property)
(call_expression . (identifier) @name) @reference.call
(call_expression . (navigation_expression (identifier) @name .)) @reference.call
(delegation_specifier [
  (user_type (identifier) @name)
  (constructor_invocation (user_type (identifier) @name))
  (explicit_delegation (user_type (identifier) @name))
]) @reference.implementation
"#;

const SWIFT_TAGS: &str = r#"
(class_declaration declaration_kind: "class" name: (type_identifier) @name) @definition.class
(class_declaration declaration_kind: "struct" name: (type_identifier) @name) @definition.struct
(class_declaration declaration_kind: "enum" name: (type_identifier) @name) @definition.enum
(class_declaration declaration_kind: "actor" name: (type_identifier) @name) @definition.actor
(protocol_declaration name: (type_identifier) @name) @definition.protocol
(typealias_declaration name: (type_identifier) @name) @definition.type
(source_file (function_declaration name: (simple_identifier) @name) @definition.function)
(class_body (function_declaration name: (simple_identifier) @name) @definition.method)
(enum_class_body (function_declaration name: (simple_identifier) @name) @definition.method)
(protocol_body
  (protocol_function_declaration name: (simple_identifier) @name) @definition.method)
(class_body (init_declaration "init" @name) @definition.constructor)
(enum_class_body (init_declaration "init" @name) @definition.constructor)
(source_file
  (property_declaration
    name: (pattern bound_identifier: (simple_identifier) @name)) @definition.property)
(class_body
  (property_declaration
    name: (pattern bound_identifier: (simple_identifier) @name)) @definition.property)
(enum_class_body
  (property_declaration
    name: (pattern bound_identifier: (simple_identifier) @name)) @definition.property)
(call_expression . (simple_identifier) @name) @reference.call
(call_expression
  . (navigation_expression
      suffix: (navigation_suffix suffix: (simple_identifier) @name))) @reference.call
(inheritance_specifier
  inherits_from: (user_type (type_identifier) @name)) @reference.implementation
"#;

const SCALA_TAGS: &str = r#"
(class_definition name: (identifier) @name) @definition.class
(object_definition name: (identifier) @name) @definition.object
(trait_definition name: (identifier) @name) @definition.trait
(enum_definition name: (identifier) @name) @definition.enum
(compilation_unit (type_definition name: (type_identifier) @name) @definition.type)
(template_body (type_definition name: (type_identifier) @name) @definition.type)
(compilation_unit (function_definition name: (identifier) @name) @definition.function)
(template_body
  [(function_definition name: (identifier) @name)
   (function_declaration name: (identifier) @name)] @definition.method)
(compilation_unit
  [(val_definition pattern: (identifier) @name)
   (var_definition pattern: (identifier) @name)] @definition.variable)
(template_body
  [(val_definition pattern: (identifier) @name)
   (var_definition pattern: (identifier) @name)
   (val_declaration name: (identifier) @name)
   (var_declaration name: (identifier) @name)] @definition.variable)
((call_expression
  function: (call_expression
    function: (identifier) @_test
    arguments: (arguments . (string) @name))
  arguments: (block)) @definition.test
  (#any-of? @_test "test" "it"))
(call_expression function: (identifier) @name) @reference.call
(call_expression function: (field_expression field: (identifier) @name)) @reference.call
(call_expression
  function: (generic_function function: (identifier) @name)) @reference.call
(instance_expression (type_identifier) @name) @reference.class
(extends_clause type: (type_identifier) @name) @reference.class
"#;

const DART_TAGS: &str = r#"
(class_declaration name: (identifier) @name) @definition.class
(mixin_declaration name: (identifier) @name) @definition.mixin
(extension_declaration name: (identifier) @name) @definition.extension
(extension_type_declaration
  name: (extension_type_name (identifier) @name)) @definition.class
(enum_declaration name: (identifier) @name) @definition.enum
(type_alias . (type_identifier) @name) @definition.type
(function_declaration
  signature: [
    (function_signature name: (identifier) @name)
    (getter_signature name: (identifier) @name)
    (setter_signature name: (identifier) @name)
  ]) @definition.function
(top_level_variable_declaration [
  (static_final_declaration_list (static_final_declaration name: (identifier) @name))
  (initialized_identifier_list (initialized_identifier name: (identifier) @name))
]) @definition.variable
(method_declaration
  signature: (method_signature [
    (function_signature name: (identifier) @name)
    (getter_signature name: (identifier) @name)
    (setter_signature name: (identifier) @name)
  ])) @definition.method
(method_declaration
  signature: (method_signature [
    (constructor_signature name: (identifier) @name . parameters: (formal_parameter_list))
    (factory_constructor_signature
      name: (identifier) @name . parameters: (formal_parameter_list))
  ])) @definition.constructor
(member_declaration
  (declaration [
    (function_signature name: (identifier) @name)
    (getter_signature name: (identifier) @name)
    (setter_signature name: (identifier) @name)
  ])) @definition.method
(member_declaration
  (declaration [
    (constructor_signature name: (identifier) @name . parameters: (formal_parameter_list))
    (constant_constructor_signature
      name: (identifier) @name . parameters: (formal_parameter_list))
    (factory_constructor_signature
      name: (identifier) @name . parameters: (formal_parameter_list))
    (redirecting_factory_constructor_signature
      name: (identifier) @name . parameters: (formal_parameter_list))
  ])) @definition.constructor
(member_declaration
  (declaration [
    (static_final_declaration_list (static_final_declaration name: (identifier) @name))
    (initialized_identifier_list (initialized_identifier name: (identifier) @name))
  ])) @definition.field
((call_expression
  function: (identifier) @_test
  arguments: (arguments . (string_literal) @name)) @definition.test
  (#any-of? @_test "test" "testWidgets"))
(call_expression function: (identifier) @name) @reference.call
(call_expression
  function: [
    (member_expression property: (identifier) @name)
    (null_aware_member_expression property: (identifier) @name)
  ]) @reference.call
(cascade_call_expression property: (identifier) @name) @reference.call
(new_expression type: (type . (type_identifier) @name)) @reference.class
(const_object_expression type: (type . (type_identifier) @name)) @reference.class
(superclass type: (type . (type_identifier) @name)) @reference.class
(interfaces (type . (type_identifier) @name)) @reference.implementation
(mixins (type . (type_identifier) @name)) @reference.implementation
"#;

// Block names follow Terraform's reference syntax. References are collected
// from expressions while walking the tree.
const HCL_TAGS: &str = r#"
((config_file (body (block . (identifier) @name) @definition.block))
  (#not-eq? @name "locals"))
((config_file
  (body
    (block . (identifier) @_locals
      (body (attribute . (identifier) @name) @definition.local))))
  (#eq? @_locals "locals"))
"#;

const LUA_TAGS: &str = r#"
(function_declaration
  name: [
    (identifier) @name
    (dot_index_expression field: (identifier) @name)
  ]) @definition.function
(function_declaration
  name: (method_index_expression method: (identifier) @name)) @definition.method
(assignment_statement
  (variable_list
    . name: [
      (identifier) @name
      (dot_index_expression field: (identifier) @name)
    ])
  (expression_list . value: (function_definition))) @definition.function
((function_call
  name: (identifier) @_test
  arguments: (arguments . (string) @name . (function_definition))) @definition.test
  (#any-of? @_test "it" "test"))
(function_call
  name: [
    (identifier) @name
    (dot_index_expression field: (identifier) @name)
    (method_index_expression method: (identifier) @name)
  ]) @reference.call
"#;

const ZIG_TAGS: &str = r#"
(source_file (variable_declaration . (identifier) @name) @definition.constant)
(struct_declaration (variable_declaration . (identifier) @name) @definition.constant)
(enum_declaration (variable_declaration . (identifier) @name) @definition.constant)
(union_declaration (variable_declaration . (identifier) @name) @definition.constant)
(function_declaration name: (identifier) @name) @definition.function
(test_declaration [(string) (identifier)] @name) @definition.test
(call_expression function: (identifier) @name) @reference.call
(call_expression function: (field_expression member: (identifier) @name)) @reference.call
"#;

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
            "kt" | "kts" => Self::Kotlin,
            "swift" => Self::Swift,
            "scala" | "sc" | "sbt" => Self::Scala,
            "dart" => Self::Dart,
            "tf" | "tfvars" | "tofu" | "hcl" => Self::Hcl,
            "lua" => Self::Lua,
            "zig" => Self::Zig,
            _ => return None,
        })
    }

    pub fn family(self) -> Self {
        match self {
            Self::TypeScript | Self::Tsx => Self::JavaScript,
            Self::Cpp => Self::C,
            // JVM languages call each other's declarations directly.
            Self::Kotlin | Self::Scala => Self::Java,
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
            Self::Kotlin => (tree_sitter_kotlin_ng::LANGUAGE, KOTLIN_TAGS),
            Self::Swift => (tree_sitter_swift::LANGUAGE, SWIFT_TAGS),
            Self::Scala => (tree_sitter_scala::LANGUAGE, SCALA_TAGS),
            Self::Dart => (tree_sitter_dart::LANGUAGE, DART_TAGS),
            Self::Hcl => (tree_sitter_hcl::LANGUAGE, HCL_TAGS),
            Self::Lua => (tree_sitter_lua::LANGUAGE, LUA_TAGS),
            Self::Zig => (tree_sitter_zig::LANGUAGE, ZIG_TAGS),
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
    /// Where the declaration itself starts, after decorators that `bytes`
    /// reaches back to.
    #[serde(skip)]
    pub signature_start: usize,
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
        definitions.retain(|tag| type_extension(grammar, tag.node, source).is_none());
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
            let (name, kind) = match (grammar, label.strip_prefix("definition.").unwrap()) {
                (Grammar::Sql, kind) => (sql_name(name), kind),
                (Grammar::Hcl, "block") => (hcl_block_name(*node, name, source), *name),
                (Grammar::Hcl, kind) => (format!("local.{name}"), kind),
                (Grammar::Zig, "constant") => (name.to_string(), zig_container(*node)),
                // Dart spells the unnamed constructor `Type.new`.
                (Grammar::Dart, "constructor") if name_node.prev_named_sibling().is_none() => {
                    ("new".to_owned(), "constructor")
                }
                // Test calls and blocks are named by their quoted description,
                // which cannot collide with the function under test.
                (_, "test") => (format!("\"{}\"", literal(name).unwrap_or(name)), "test"),
                (_, kind) => (name.to_string(), kind),
            };
            // Declarators that share one statement are siblings, not owners.
            let parent = (0..id).rev().find(|&parent| {
                let owner = definitions[parent].node;
                owner.end_byte() >= node.end_byte() && owner.byte_range() != node.byte_range()
            });
            let extension = matches!(grammar, Grammar::Swift | Grammar::Scala)
                .then(|| {
                    std::iter::successors(node.parent(), Node::parent).find_map(|ancestor| {
                        Some((ancestor, type_extension(grammar, ancestor, source)?))
                    })
                })
                .flatten();
            let owner = match (parent, extension) {
                (Some(parent), Some((extension, extended)))
                    if definitions[parent].node.start_byte() <= extension.start_byte()
                        && extension.end_byte() <= definitions[parent].node.end_byte() =>
                {
                    Some(format!("{}::{extended}", facts.definitions[parent].path))
                }
                (Some(parent), _) => Some(facts.definitions[parent].path.clone()),
                (None, extension) => extension.map(|(_, extended)| extended),
            };
            let path = owner.map_or_else(|| name.clone(), |owner| format!("{owner}::{name}"));
            let callable = matches!(kind, "function" | "method");
            let test = kind == "test"
                || match grammar {
                    Grammar::Python => name.starts_with("test_"),
                    Grammar::Go => name
                        .strip_prefix("Test")
                        .is_some_and(|suffix| suffix.starts_with(char::is_uppercase)),
                    Grammar::Java | Grammar::Kotlin | Grammar::Scala => {
                        callable && test_annotation(*node, source)
                    }
                    // XCTest runs methods named test…; Swift Testing marks them @Test.
                    Grammar::Swift => {
                        (kind == "method" && name.starts_with("test"))
                            || (callable && test_annotation(*node, source))
                    }
                    _ => false,
                };
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
            } else if kind == "test" && matches!(node.kind(), "call_expression" | "function_call") {
                // A test call's header is the line that names it.
                source[name_node.end_byte()..extent.end_byte()]
                    .find('\n')
                    .map(|offset| name_node.end_byte() + offset)
            } else {
                None
            };
            facts.definitions.push(Definition {
                bytes: decorated_start(extent)..extent.end_byte(),
                signature_start: extent.start_byte(),
                header_end: header_end.unwrap_or_else(|| {
                    body.or_else(|| extent.child_by_field_name("body"))
                        .or_else(|| header_boundary(grammar, extent))
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
                    | "import_or_export"
            ) || (grammar == Grammar::Kotlin && node.kind() == "import" && node.is_named())
            {
                facts.import_ranges.push(node.byte_range());
            }
            if grammar == Grammar::Hcl
                && node.kind() == "variable_expr"
                && let Some(name) = hcl_reference(node, source)
            {
                let line = node.start_position().row + 1;
                let from = parent_symbol(&definitions, node);
                if seen.insert((Kind::Reference, line, from, name.clone())) {
                    facts.references.push(Reference {
                        name,
                        from,
                        line,
                        kind: Kind::Reference,
                    });
                }
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
                matches!(node.kind(), "variable_declarator" | "assignment_statement")
                    && parent.named_child_count() == 1
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

// TypeScript keeps a class member's decorators as preceding siblings in the
// class body rather than inside the member, so the span reaches back to them.
fn decorated_start(node: Node<'_>) -> usize {
    let mut start = node.start_byte();
    let mut sibling = node.prev_named_sibling();
    while let Some(previous) = sibling {
        match previous.kind() {
            "decorator" => start = previous.start_byte(),
            "comment" => {}
            _ => break,
        }
        sibling = previous.prev_named_sibling();
    }
    start
}

fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    source.get(node.byte_range()).unwrap_or("")
}

// Grammars without a body field still need a signature that stops before the
// declaration's body.
fn header_boundary(grammar: Grammar, node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    let mut children = node.named_children(&mut cursor);
    match grammar {
        Grammar::Kotlin | Grammar::Hcl | Grammar::Zig => {
            children.find_map(|child| match child.kind() {
                "class_body" | "enum_class_body" | "function_body" | "block_start" | "block" => {
                    Some(child)
                }
                "struct_declaration" | "enum_declaration" | "union_declaration" => {
                    let mut cursor = child.walk();
                    child
                        .children(&mut cursor)
                        .find(|brace| brace.kind() == "{")
                }
                _ => None,
            })
        }
        _ => None,
    }
}

// Swift extensions and Scala companion objects add members to a type declared
// beside them, as Rust impl blocks do. They are owners, not symbols, so a
// query for the type stays unambiguous.
fn type_extension(grammar: Grammar, node: Node<'_>, source: &str) -> Option<String> {
    let name = match (grammar, node.kind()) {
        (Grammar::Swift, "class_declaration") => {
            (node.child_by_field_name("declaration_kind")?.kind() == "extension")
                .then_some(text(node.child_by_field_name("name")?, source))
        }
        (Grammar::Scala, "object_definition") => {
            let name = text(node.child_by_field_name("name")?, source);
            let scope = node.parent()?;
            let mut cursor = scope.walk();
            let companion = scope.named_children(&mut cursor).any(|sibling| {
                matches!(
                    sibling.kind(),
                    "class_definition" | "trait_definition" | "enum_definition"
                ) && sibling
                    .child_by_field_name("name")
                    .is_some_and(|other| text(other, source) == name)
            });
            companion.then_some(name)
        }
        _ => None,
    };
    name.map(str::to_owned)
}

// JUnit, kotlin.test, ScalaTest's JUnit runner and Swift Testing mark tests
// with an annotation or attribute named Test, ParameterizedTest and so on.
fn test_annotation(node: Node<'_>, source: &str) -> bool {
    let mut cursor = node.walk();
    let mut annotations = Vec::new();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "modifiers" {
            let mut cursor = child.walk();
            annotations.extend(child.named_children(&mut cursor));
        } else {
            annotations.push(child);
        }
    }
    annotations.into_iter().any(|annotation| {
        matches!(
            annotation.kind(),
            "annotation" | "marker_annotation" | "attribute"
        ) && text(annotation, source)
            .trim_start_matches('@')
            .split(|c: char| c == '(' || c.is_whitespace())
            .next()
            .and_then(|name| name.rsplit('.').next())
            .is_some_and(|name| name.ends_with("Test"))
    })
}

// Terraform addresses: `aws_s3_bucket.logs`, `data.T.N`, `var.x`, `module.y`.
// Other top-level blocks join their type and labels the same way.
fn hcl_block_name(node: Node<'_>, kind: &str, source: &str) -> String {
    let mut cursor = node.walk();
    let labels = node
        .named_children(&mut cursor)
        .skip(1)
        .take_while(|child| matches!(child.kind(), "string_lit" | "identifier"))
        .map(|label| {
            let label = text(label, source);
            label
                .strip_prefix('"')
                .and_then(|label| label.strip_suffix('"'))
                .unwrap_or(label)
        });
    let mut parts = match kind {
        "resource" => Vec::new(),
        "variable" => vec!["var"],
        other => vec![other],
    };
    parts.extend(labels);
    if parts.is_empty() {
        parts.push(kind);
    }
    parts.join(".")
}

fn hcl_reference(node: Node<'_>, source: &str) -> Option<String> {
    let root = text(node, source);
    let width = match root {
        "count" | "each" | "self" | "path" | "terraform" => return None,
        "data" => 3,
        _ => 2,
    };
    let mut parts = vec![root];
    let mut next = node.next_named_sibling();
    while parts.len() < width
        && let Some(attribute) = next.filter(|next| next.kind() == "get_attr")
    {
        parts.push(text(attribute.named_child(0)?, source));
        next = attribute.next_named_sibling();
    }
    (parts.len() == width).then(|| parts.join("."))
}

fn zig_container(node: Node<'_>) -> &'static str {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find_map(|child| match child.kind() {
            "struct_declaration" => Some("struct"),
            "enum_declaration" => Some("enum"),
            "union_declaration" => Some("union"),
            "opaque_declaration" => Some("opaque"),
            "error_set_declaration" => Some("error"),
            _ => None,
        })
        .unwrap_or("constant")
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
