use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;
use std::sync::LazyLock;

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
    Elixir,
    ObjectiveC,
    PowerShell,
    Julia,
    Groovy,
    Vue,
    Svelte,
    Astro,
}

const SQL_TAGS: &str = "
(create_table (object_reference) @name) @definition.table
(create_view (object_reference) @name) @definition.view
(create_materialized_view (object_reference) @name) @definition.view
(create_function (keyword_function) . (object_reference) @name) @definition.function
(create_procedure [(keyword_procedure) (keyword_exists)] . (object_reference) @name) @definition.procedure
(create_trigger [(keyword_trigger) (keyword_exists)] . (object_reference) @name) @definition.trigger
(create_type (keyword_type) . (object_reference) @name) @definition.type
(create_schema [(keyword_schema) (keyword_exists)] . (identifier) @name) @definition.schema
(create_schema (keyword_schema) . (keyword_authorization) . (identifier) @name) @definition.schema
(relation (object_reference) @name) @reference.table
(insert (object_reference) @name) @reference.table
(from (object_reference) @name) @reference.table
(alter_table (object_reference) @name) @reference.table
(drop_table (object_reference) @name) @reference.table
(create_index (object_reference) @name) @reference.table
(create_trigger (keyword_on) . (object_reference) @name) @reference.table
";

// Jest/Vitest/Mocha/Playwright suites and test blocks, including modifier and
// table-driven `.each`/`.for` forms.
const JS_TEST_TAGS: &str = r#"
((call_expression
  function: (identifier) @_test
  arguments: (arguments . [(string) (template_string)] @name . (_))) @definition.test
  (#any-of? @_test "describe" "it" "test"))
((call_expression
  function: (member_expression
    object: (identifier) @_test
    property: (property_identifier) @_mod)
  arguments: (arguments . [(string) (template_string)] @name)) @definition.test
  (#any-of? @_test "describe" "it" "test")
  (#any-of? @_mod "only" "skip" "todo" "concurrent" "sequential" "fails" "describe"))
((call_expression
  function: (member_expression
    object: (member_expression
      object: (identifier) @_test
      property: (property_identifier) @_sub)
    property: (property_identifier) @_mod)
  arguments: (arguments . [(string) (template_string)] @name)) @definition.test
  (#any-of? @_test "describe" "it" "test")
  (#any-of? @_sub "describe" "only" "skip" "concurrent" "sequential")
  (#any-of? @_mod "only" "skip" "todo" "concurrent" "sequential" "fails" "serial" "parallel"))
((call_expression
  function: (call_expression
    function: (member_expression
      object: (identifier) @_test
      property: (property_identifier) @_each))
  arguments: (arguments . [(string) (template_string)] @name)) @definition.test
  (#any-of? @_test "describe" "it" "test")
  (#any-of? @_each "each" "for"))
((call_expression
  function: (call_expression
    function: (member_expression
      object: (member_expression
        object: (identifier) @_test
        property: (property_identifier) @_mod)
      property: (property_identifier) @_each))
  arguments: (arguments . [(string) (template_string)] @name)) @definition.test
  (#any-of? @_test "describe" "it" "test")
  (#any-of? @_mod "only" "skip" "concurrent" "sequential" "describe")
  (#any-of? @_each "each" "for"))
"#;

// RSpec example groups and examples (`describe`, `context`, `it`, `specify`),
// both bare and under `RSpec.describe`.
const RUBY_TEST_TAGS: &str = r#"
((call
  !receiver
  method: (identifier) @_test
  arguments: (argument_list . [(string) (constant) (scope_resolution)] @name)
  block: [(do_block) (block)]) @definition.test
  (#any-of? @_test
    "describe" "context" "it" "specify"
    "xdescribe" "fdescribe" "xcontext" "fcontext"
    "xit" "fit" "xspecify" "fspecify"))
((call
  receiver: [
    (constant) @_rspec
    (scope_resolution name: (constant) @_rspec)
  ]
  method: (identifier) @_test
  arguments: (argument_list . [(string) (constant) (scope_resolution)] @name)
  block: [(do_block) (block)]) @definition.test
  (#eq? @_rspec "RSpec")
  (#any-of? @_test "describe" "fdescribe" "xdescribe" "shared_examples" "shared_context"))
"#;

// C# declarations, base/interface inheritance, attributes, and direct, generic,
// and namespace-qualified calls (capturing both final receiver and method name).
const CSHARP_TAGS: &str = r#"
(namespace_declaration name: (_) @name) @definition.module
(class_declaration name: (identifier) @name) @definition.class
(struct_declaration name: (identifier) @name) @definition.struct
(interface_declaration name: (identifier) @name) @definition.interface
(enum_declaration name: (identifier) @name) @definition.enum
(record_declaration name: (identifier) @name) @definition.record
(delegate_declaration name: (identifier) @name) @definition.type
(method_declaration name: (identifier) @name) @definition.method
(constructor_declaration name: (identifier) @name) @definition.constructor
(property_declaration name: (identifier) @name) @definition.property

(base_list [
  (identifier) @name
  (generic_name (identifier) @name)
  (qualified_name name: [(identifier) @name (generic_name (identifier) @name)])
  (alias_qualified_name name: [(identifier) @name (generic_name (identifier) @name)])
  (primary_constructor_base_type
    type: [
      (identifier) @name
      (generic_name (identifier) @name)
      (qualified_name name: [(identifier) @name (generic_name (identifier) @name)])
      (alias_qualified_name name: [(identifier) @name (generic_name (identifier) @name)])
    ])
]) @reference.implementation

(attribute
  name: [
    (identifier) @name
    (generic_name (identifier) @name)
    (qualified_name name: [(identifier) @name (generic_name (identifier) @name)])
    (alias_qualified_name name: [(identifier) @name (generic_name (identifier) @name)])
  ]) @reference.type

(object_creation_expression
  type: [
    (identifier) @name
    (generic_name (identifier) @name)
    (qualified_name name: [(identifier) @name (generic_name (identifier) @name)])
    (alias_qualified_name name: [(identifier) @name (generic_name (identifier) @name)])
  ]) @reference.class

(type_parameter_constraints_clause (identifier) @name) @reference.class
(type_parameter_constraint
  type: [
    (identifier) @name
    (generic_name (identifier) @name)
    (qualified_name name: [(identifier) @name (generic_name (identifier) @name)])
  ]) @reference.class
(variable_declaration
  type: [
    (identifier) @name
    (generic_name (identifier) @name)
    (qualified_name name: [(identifier) @name (generic_name (identifier) @name)])
  ]) @reference.class

(invocation_expression
  function: [
    (identifier) @name
    (generic_name (identifier) @name)
  ]) @reference.call
(invocation_expression
  function: (member_access_expression
    name: [
      (identifier) @name
      (generic_name (identifier) @name)
    ])) @reference.send
(invocation_expression
  function: (conditional_access_expression
    (member_binding_expression
      name: [
        (identifier) @name
        (generic_name (identifier) @name)
      ]))) @reference.send
(invocation_expression
  function: (member_access_expression
    expression: [
      (member_access_expression
        name: [(identifier) @name (generic_name (identifier) @name)])
      (qualified_name
        name: [(identifier) @name (generic_name (identifier) @name)])
      (alias_qualified_name
        name: [(identifier) @name (generic_name (identifier) @name)])
      (generic_name (identifier) @name)
    ])) @reference.class
((invocation_expression
  function: (member_access_expression
    expression: (identifier) @name)) @reference.class
  (#match? @name "^[A-Z]"))
"#;

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

// Elixir declares everything with macro calls. A function head is the call
// that names it, and ExUnit tests are `test "…" do` calls.
const ELIXIR_TAGS: &str = r#"
((call target: (identifier) @_def (arguments . (alias) @name)) @definition.module
  (#eq? @_def "defmodule"))
((call target: (identifier) @_def (arguments . (alias) @name)) @definition.protocol
  (#eq? @_def "defprotocol"))
((call
  target: (identifier) @_def
  (arguments
    . [
      (identifier) @name
      (call target: (identifier) @name)
      (binary_operator left: (call target: (identifier) @name) operator: "when")
    ])) @definition.function
  (#any-of? @_def "def" "defp" "defdelegate" "defguard" "defguardp" "defn" "defnp"))
((call
  target: (identifier) @_def
  (arguments
    . [
      (identifier) @name
      (call target: (identifier) @name)
      (binary_operator left: (call target: (identifier) @name) operator: "when")
    ])) @definition.macro
  (#any-of? @_def "defmacro" "defmacrop"))
((unary_operator
  operator: "@"
  operand: (call
    target: (identifier) @_type
    (arguments
      (binary_operator
        left: [(identifier) @name (call target: (identifier) @name)]
        operator: "::")))) @definition.type
  (#any-of? @_type "type" "typep" "opaque"))
((call target: (identifier) @_test (arguments . (string) @name)) @definition.test
  (#eq? @_test "test"))
((call target: (identifier) @_use (arguments . (alias) @name)) @reference.module
  (#any-of? @_use "alias" "import" "require" "use"))
((call target: (identifier) @name) @reference.call
  (#not-any-of? @name
    "def" "defp" "defdelegate" "defguard" "defguardp" "defn" "defnp" "defmacro"
    "defmacrop" "defmodule" "defprotocol" "defimpl" "defstruct" "defexception"
    "defoverridable" "alias" "import" "require" "use" "case" "cond" "if" "unless"
    "for" "with" "try" "receive" "quote" "unquote" "unquote_splicing" "raise"
    "reraise" "throw" "super" "fn" "describe" "test" "setup" "setup_all"))
(call target: (dot right: (identifier) @name)) @reference.call
(binary_operator operator: "|>" right: (identifier) @name) @reference.call
"#;

// Methods are named by their first selector keyword, as messages are, and an
// `@implementation` or class extension adds members to the class it names.
const OBJECTIVE_C_TAGS: &str = r#"
(class_interface . (identifier) @name) @definition.class
(protocol_declaration . (identifier) @name) @definition.protocol
(class_interface
  (property_declaration
    (struct_declaration
      (struct_declarator
        [(identifier) @name (pointer_declarator declarator: (identifier) @name)])))
    @definition.property)
(protocol_declaration
  (property_declaration
    (struct_declaration
      (struct_declarator
        [(identifier) @name (pointer_declarator declarator: (identifier) @name)])))
    @definition.property)
(protocol_declaration
  (method_declaration (method_type) . (identifier) @name) @definition.method)
(method_definition (method_type) . (identifier) @name) @definition.method
(method_definition . (identifier) @name) @definition.method
(message_expression receiver: (_) . method: (identifier) @name) @reference.call
"#;

// Pester tests are `It '…' { }` commands.
const POWERSHELL_TAGS: &str = r#"
(function_statement (function_name) @name) @definition.function
(class_statement . (simple_name) @name) @definition.class
(enum_statement . (simple_name) @name) @definition.enum
(class_method_definition (simple_name) @name) @definition.method
(class_property_definition (variable) @name) @definition.property
((command
  command_name: (command_name) @_it
  command_elements: (command_elements . (command_argument_sep) . (_) @name)) @definition.test
  (#any-of? @_it "It" "it" "IT"))
(command command_name: (command_name) @name) @reference.call
(invokation_expression (member_name (simple_name) @name)) @reference.call
"#;

// A Julia signature may carry a return type and a `where` clause around the
// call that names the function.
const JULIA_TAGS: &str = r#"
(module_definition name: (identifier) @name) @definition.module
(struct_definition
  (type_head
    . [
      (identifier) @name
      (parametrized_type_expression . (identifier) @name)
      (binary_expression
        . [(identifier) @name (parametrized_type_expression . (identifier) @name)])
    ])) @definition.struct
(abstract_definition
  (type_head
    . [
      (identifier) @name
      (parametrized_type_expression . (identifier) @name)
      (binary_expression
        . [(identifier) @name (parametrized_type_expression . (identifier) @name)])
    ])) @definition.type
(function_definition
  (signature
    . [
      (call_expression . [(identifier) @name (field_expression (identifier) @name .)])
      (typed_expression
        . (call_expression . [(identifier) @name (field_expression (identifier) @name .)]))
      (where_expression
        . [
          (call_expression . [(identifier) @name (field_expression (identifier) @name .)])
          (typed_expression
            . (call_expression . [(identifier) @name (field_expression (identifier) @name .)]))
        ])
    ])) @definition.function
(assignment
  . [
    (call_expression . [(identifier) @name (field_expression (identifier) @name .)])
    (typed_expression
      . (call_expression . [(identifier) @name (field_expression (identifier) @name .)]))
    (where_expression
      . [
        (call_expression . [(identifier) @name (field_expression (identifier) @name .)])
        (typed_expression
          . (call_expression . [(identifier) @name (field_expression (identifier) @name .)]))
      ])
  ]) @definition.function
(macro_definition (signature . (call_expression . (identifier) @name))) @definition.macro
(const_statement (assignment . (identifier) @name)) @definition.constant
((macrocall_expression
  (macro_identifier (identifier) @_testset)
  (macro_argument_list . (string_literal) @name)) @definition.test
  (#eq? @_testset "testset"))
(call_expression
  . [(identifier) @name (field_expression (identifier) @name .)]) @reference.call
(broadcast_call_expression
  . [(identifier) @name (field_expression (identifier) @name .)]) @reference.call
(macrocall_expression (macro_identifier (identifier) @name)) @reference.call
"#;

// Spock names feature methods with strings, and Gradle registers tasks by name.
const GROOVY_TAGS: &str = r#"
(class_declaration name: (identifier) @name) @definition.class
(interface_declaration name: (identifier) @name) @definition.interface
(trait_declaration name: (identifier) @name) @definition.trait
(enum_declaration name: (identifier) @name) @definition.enum
(class_body (method_declaration name: (identifier) @name) @definition.method)
(class_body (method_declaration name: (quoted_identifier) @name) @definition.test)
(class_body (constructor_declaration name: (identifier) @name) @definition.constructor)
(class_body (field_declaration (variable_declarator name: (identifier) @name)) @definition.field)
(source_file (method_declaration name: (identifier) @name) @definition.function)
((method_invocation
  function: (field_access object: (identifier) @_tasks field: (identifier) @_register)
  arguments: (argument_list . (string_literal) @name)) @definition.task
  (#eq? @_tasks "tasks")
  (#any-of? @_register "register" "create"))
(method_invocation function: (identifier) @name) @reference.call
(method_invocation function: (field_access field: (identifier) @name)) @reference.call
(object_creation_expression (type_identifier) @name) @reference.class
"#;

impl Grammar {
    pub fn for_file(file: &str) -> Option<Self> {
        let path = Path::new(file);
        let raw_ext = path.extension()?.to_str()?;
        let mut buf = [0u8; 12];
        if raw_ext.len() > buf.len() || !raw_ext.is_ascii() {
            return None;
        }
        for (dst, src) in buf.iter_mut().zip(raw_ext.bytes()) {
            *dst = src.to_ascii_lowercase();
        }
        let extension = std::str::from_utf8(&buf[..raw_ext.len()]).ok()?;
        Some(match extension {
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
            "ex" | "exs" => Self::Elixir,
            // Not `.mm`: in Objective-C++ from MMKV, Texture and Realm, this
            // grammar found errors in 209 of 357 files, against 48 of 614 `.m`.
            "m" => Self::ObjectiveC,
            "ps1" | "psm1" => Self::PowerShell,
            "jl" => Self::Julia,
            "groovy" | "gvy" | "gradle" => Self::Groovy,
            "vue" => Self::Vue,
            "svelte" => Self::Svelte,
            "astro" => Self::Astro,
            _ => return None,
        })
    }

    /// The grammar for a file from its name and text: an extensionless script
    /// takes the language its `#!` line names, and Objective-C headers and
    /// sources declare themselves with `@interface` or `#import`. MATLAB and
    /// Octave also use `.m`, so a `.m` file without those markers has none.
    pub fn detect(file: &str, text: &str) -> Option<Self> {
        let path = Path::new(file);
        match Self::for_file(file) {
            Some(Self::C) if path.extension().is_some_and(|e| e == "h") && objective_c(text) => {
                Some(Self::ObjectiveC)
            }
            Some(Self::ObjectiveC) if !objective_c(text) => None,
            None if path.extension().is_none() => {
                if path.file_name().is_some_and(|name| name == "Jenkinsfile") {
                    return Some(Self::Groovy);
                }
                Self::for_interpreter(&crate::exec::Shebang::parse(text.as_bytes())?.program)
            }
            grammar => grammar,
        }
    }

    /// The grammar for an interpreter a `#!` line names, such as `python3.12`.
    fn for_interpreter(program: &str) -> Option<Self> {
        Some(
            match program.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.') {
                "sh" | "bash" | "zsh" | "dash" | "ksh" | "ash" => Self::Bash,
                "python" | "pypy" => Self::Python,
                "node" | "nodejs" => Self::JavaScript,
                "deno" | "bun" | "ts-node" | "tsx" => Self::TypeScript,
                "ruby" | "jruby" => Self::Ruby,
                "php" => Self::Php,
                "lua" | "luajit" => Self::Lua,
                "elixir" => Self::Elixir,
                "pwsh" | "powershell" => Self::PowerShell,
                "julia" => Self::Julia,
                "groovy" => Self::Groovy,
                _ => return None,
            },
        )
    }

    /// The language's name in reports such as `map`.
    pub fn name(self) -> &'static str {
        match self {
            Self::JavaScript => "JavaScript",
            Self::TypeScript => "TypeScript",
            Self::Tsx => "TSX",
            Self::Python => "Python",
            Self::Go => "Go",
            Self::Java => "Java",
            Self::C => "C",
            Self::Cpp => "C++",
            Self::CSharp => "C#",
            Self::Ruby => "Ruby",
            Self::Php => "PHP",
            Self::Bash => "Bash",
            Self::Sql => "SQL",
            Self::Kotlin => "Kotlin",
            Self::Swift => "Swift",
            Self::Scala => "Scala",
            Self::Dart => "Dart",
            Self::Hcl => "HCL",
            Self::Lua => "Lua",
            Self::Zig => "Zig",
            Self::Elixir => "Elixir",
            Self::ObjectiveC => "Objective-C",
            Self::PowerShell => "PowerShell",
            Self::Julia => "Julia",
            Self::Groovy => "Groovy",
            Self::Vue => "Vue",
            Self::Svelte => "Svelte",
            Self::Astro => "Astro",
        }
    }

    pub fn family(self) -> Self {
        match self {
            Self::TypeScript | Self::Tsx | Self::Vue | Self::Svelte | Self::Astro => {
                Self::JavaScript
            }
            Self::Cpp | Self::ObjectiveC => Self::C,
            // JVM languages call each other's declarations directly.
            Self::Kotlin | Self::Scala | Self::Groovy => Self::Java,
            other => other,
        }
    }

    /// The Tree-sitter language alone, without compiling the tags query.
    pub fn language(self) -> Language {
        match self {
            Self::JavaScript => tree_sitter_javascript::LANGUAGE,
            // Components are parsed as the TypeScript in their scripts.
            Self::TypeScript | Self::Vue | Self::Svelte | Self::Astro => {
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT
            }
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX,
            Self::Python => tree_sitter_python::LANGUAGE,
            Self::Go => tree_sitter_go::LANGUAGE,
            Self::Java => tree_sitter_java::LANGUAGE,
            Self::C => tree_sitter_c::LANGUAGE,
            Self::Cpp => tree_sitter_cpp::LANGUAGE,
            Self::CSharp => tree_sitter_c_sharp::LANGUAGE,
            Self::Ruby => tree_sitter_ruby::LANGUAGE,
            Self::Php => tree_sitter_php::LANGUAGE_PHP,
            Self::Bash => tree_sitter_bash::LANGUAGE,
            Self::Sql => tree_sitter_sequel::LANGUAGE,
            Self::Kotlin => tree_sitter_kotlin_ng::LANGUAGE,
            Self::Swift => tree_sitter_swift::LANGUAGE,
            Self::Scala => tree_sitter_scala::LANGUAGE,
            Self::Dart => tree_sitter_dart::LANGUAGE,
            Self::Hcl => tree_sitter_hcl::LANGUAGE,
            Self::Lua => tree_sitter_lua::LANGUAGE,
            Self::Zig => tree_sitter_zig::LANGUAGE,
            Self::Elixir => tree_sitter_elixir::LANGUAGE,
            Self::ObjectiveC => tree_sitter_objc::LANGUAGE,
            Self::PowerShell => tree_sitter_powershell::LANGUAGE,
            Self::Julia => tree_sitter_julia::LANGUAGE,
            Self::Groovy => dekobon_tree_sitter_groovy::LANGUAGE,
        }
        .into()
    }

    fn canonical_query(self) -> Self {
        match self {
            Self::Vue | Self::Svelte | Self::Astro => Self::TypeScript,
            other => other,
        }
    }

    fn tags_source(self) -> String {
        let base = match self {
            Self::JavaScript => tree_sitter_javascript::TAGS_QUERY,
            Self::TypeScript | Self::Tsx | Self::Vue | Self::Svelte | Self::Astro => {
                tree_sitter_typescript::TAGS_QUERY
            }
            Self::Python => tree_sitter_python::TAGS_QUERY,
            Self::Go => tree_sitter_go::TAGS_QUERY,
            Self::Java => tree_sitter_java::TAGS_QUERY,
            Self::C => tree_sitter_c::TAGS_QUERY,
            Self::Cpp => tree_sitter_cpp::TAGS_QUERY,
            Self::CSharp => CSHARP_TAGS,
            Self::Ruby => tree_sitter_ruby::TAGS_QUERY,
            Self::Php => tree_sitter_php::TAGS_QUERY,
            Self::Bash => {
                "
                (function_definition name: (word) @name) @definition.function
                (command name: (command_name (word) @name)) @reference.call"
            }
            Self::Sql => SQL_TAGS,
            Self::Kotlin => KOTLIN_TAGS,
            Self::Swift => SWIFT_TAGS,
            Self::Scala => SCALA_TAGS,
            Self::Dart => DART_TAGS,
            Self::Hcl => HCL_TAGS,
            Self::Lua => LUA_TAGS,
            Self::Zig => ZIG_TAGS,
            Self::Elixir => ELIXIR_TAGS,
            Self::ObjectiveC => OBJECTIVE_C_TAGS,
            Self::PowerShell => POWERSHELL_TAGS,
            Self::Julia => JULIA_TAGS,
            Self::Groovy => GROOVY_TAGS,
        };
        let mut tags = base.to_owned();
        if matches!(
            self,
            Self::TypeScript | Self::Tsx | Self::Vue | Self::Svelte | Self::Astro
        ) {
            tags.push_str(tree_sitter_javascript::TAGS_QUERY);
            tags.push_str(
                "\n(type_alias_declaration name: (_) @name) @definition.type
                (enum_declaration name: (_) @name) @definition.enum
                (class_declaration name: (type_identifier) @name) @definition.class\n",
            );
        }
        if self.family() == Self::JavaScript {
            tags.push_str(JS_TEST_TAGS);
        }
        if self == Self::Ruby {
            tags.push_str(RUBY_TEST_TAGS);
        }
        if matches!(self, Self::C | Self::Cpp | Self::ObjectiveC) {
            tags.push_str("\n(call_expression function: (identifier) @name) @reference.call\n");
        }
        if self == Self::ObjectiveC {
            tags.push_str(tree_sitter_c::TAGS_QUERY);
        }
        tags
    }

    /// Compiled Tree-sitter language and tags query, cached once per grammar
    /// for the life of the process so repeated `Parsers::default()` calls never
    /// recompile queries.
    fn compiled(self) -> anyhow::Result<&'static CompiledQuery> {
        macro_rules! cached {
            ($variant:ident) => {{
                static CACHE: LazyLock<Result<CompiledQuery, String>> =
                    LazyLock::new(|| CompiledQuery::compile(Grammar::$variant));
                &*CACHE
            }};
        }
        let entry = match self {
            Self::JavaScript => cached!(JavaScript),
            Self::TypeScript | Self::Vue | Self::Svelte | Self::Astro => {
                cached!(TypeScript)
            }
            Self::Tsx => cached!(Tsx),
            Self::Python => cached!(Python),
            Self::Go => cached!(Go),
            Self::Java => cached!(Java),
            Self::C => cached!(C),
            Self::Cpp => cached!(Cpp),
            Self::CSharp => cached!(CSharp),
            Self::Ruby => cached!(Ruby),
            Self::Php => cached!(Php),
            Self::Bash => cached!(Bash),
            Self::Sql => cached!(Sql),
            Self::Kotlin => cached!(Kotlin),
            Self::Swift => cached!(Swift),
            Self::Scala => cached!(Scala),
            Self::Dart => cached!(Dart),
            Self::Hcl => cached!(Hcl),
            Self::Lua => cached!(Lua),
            Self::Zig => cached!(Zig),
            Self::Elixir => cached!(Elixir),
            Self::ObjectiveC => cached!(ObjectiveC),
            Self::PowerShell => cached!(PowerShell),
            Self::Julia => cached!(Julia),
            Self::Groovy => cached!(Groovy),
        };
        entry.as_ref().map_err(|error| anyhow::anyhow!("{error}"))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reference {
    pub name: String,
    pub from: Option<usize>,
    pub line: usize,
    pub kind: Kind,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Import {
    pub path: String,
    pub line: usize,
    pub bindings: BTreeMap<String, String>,
}

impl Import {
    /// Clean normalized module stem for display or alias/barrel matching:
    /// strips leading `./`, path-alias prefixes (`@/`, `~/`, `#/`), trailing
    /// `/index` barrel segments, and JS/TS file extensions.
    #[allow(dead_code)]
    pub fn module_stem(&self) -> String {
        normalize_js_module_stem(&self.path)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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

struct RawDef<'a> {
    name: &'a str,
    node: Node<'a>,
    name_node: Node<'a>,
    kind: &'static str,
}

struct RawRef<'a> {
    name: &'a str,
    node: Node<'a>,
    name_node: Node<'a>,
    kind: Kind,
}

enum CaptureRole {
    Ignore,
    Name,
    Definition(Box<str>),
    Reference(Kind),
}

struct CompiledQuery {
    language: Language,
    query: Query,
    name_index: Option<u32>,
    capture_roles: Vec<CaptureRole>,
}

impl CompiledQuery {
    fn compile(grammar: Grammar) -> Result<Self, String> {
        let language = grammar.language();
        let tags = grammar.tags_source();
        let query = Query::new(&language, &tags).map_err(|error| error.to_string())?;
        let name_index = query.capture_index_for_name("name");
        let capture_roles = query
            .capture_names()
            .iter()
            .map(|&name| {
                if name == "name" {
                    CaptureRole::Name
                } else if let Some(kind) = name.strip_prefix("definition.") {
                    CaptureRole::Definition(kind.into())
                } else if name.starts_with("reference.") {
                    let kind = match name {
                        "reference.call" | "reference.send" => Kind::Call,
                        "reference.table" => Kind::Data,
                        _ => Kind::Reference,
                    };
                    CaptureRole::Reference(kind)
                } else {
                    CaptureRole::Ignore
                }
            })
            .collect();
        Ok(Self {
            language,
            query,
            name_index,
            capture_roles,
        })
    }
}

#[derive(Default)]
pub struct Parsers {
    parser: Option<Parser>,
    language: Option<Grammar>,
    cursor: Option<QueryCursor>,
}

impl Parsers {
    pub fn parse(&mut self, grammar: Grammar, source: &str) -> anyhow::Result<Facts> {
        let (syntax, text) = match grammar {
            Grammar::Vue | Grammar::Svelte | Grammar::Astro => {
                let (text, jsx) = crate::components::scripts(source, grammar == Grammar::Astro);
                (
                    if jsx {
                        Grammar::Tsx
                    } else {
                        Grammar::TypeScript
                    },
                    text,
                )
            }
            Grammar::ObjectiveC => (grammar, blank_bare_macros(source)),
            _ => return self.parse_text(grammar, source),
        };
        let mut facts = self.parse_text(syntax, &text)?;
        facts.grammar = grammar;
        Ok(facts)
    }

    fn parse_text(&mut self, grammar: Grammar, source: &str) -> anyhow::Result<Facts> {
        let compiled = grammar.compiled()?;
        let canonical = grammar.canonical_query();
        let parser = self.parser.get_or_insert_with(Parser::new);
        if self.language != Some(canonical) {
            self.language = None;
            parser.set_language(&compiled.language)?;
            self.language = Some(canonical);
        }
        // PostgreSQL dollar-quoted FUNCTION/PROCEDURE bodies often hold PL/pgSQL
        // or multi-statement SQL that tree-sitter-sequel truncates or errors on.
        // Scan them once (fast-path skipped when `$` is absent), mask their
        // interior with equal-byte-length valid SQL for Tree-sitter so trailing
        // DDL parses cleanly, then restore exact spans and body table refs.
        let sql_routines = if grammar == Grammar::Sql && source.contains('$') {
            scan_sql_dollar_routines(source)
        } else {
            Vec::new()
        };
        let masked_sql = (!sql_routines.is_empty())
            .then(|| mask_sql_dollar_bodies(source, &sql_routines))
            .flatten();
        let parse_input = masked_sql.as_deref().unwrap_or(source);
        let tree = parser
            .parse(parse_input, None)
            .ok_or_else(|| anyhow::anyhow!("parse cancelled"))?;
        let query = &compiled.query;
        let roles = &compiled.capture_roles;
        let mut definitions = Vec::new();
        let mut references = Vec::new();
        if let Some(name_index) = compiled.name_index {
            let cursor = self.cursor.get_or_insert_with(QueryCursor::new);
            let mut matches = cursor.matches(query, tree.root_node(), parse_input.as_bytes());
            while let Some(matched) = matches.next() {
                let Some(name) = matched.captures.iter().find(|cap| cap.index == name_index) else {
                    continue;
                };
                let name_text = text(name.node, source);
                if name_text.is_empty() {
                    continue;
                }
                for capture in matched.captures {
                    match &roles[capture.index as usize] {
                        CaptureRole::Definition(kind) => {
                            definitions.push(RawDef {
                                name: name_text,
                                node: capture.node,
                                name_node: name.node,
                                kind,
                            });
                        }
                        CaptureRole::Reference(kind) => {
                            references.push(RawRef {
                                name: name_text,
                                node: capture.node,
                                name_node: name.node,
                                kind: *kind,
                            });
                        }
                        CaptureRole::Ignore | CaptureRole::Name => {}
                    }
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
        definitions.retain(|tag| {
            type_extension(grammar, tag.node, source).is_none()
                // A Pester `It` names its test with a string, not a parameter.
                && (grammar != Grammar::PowerShell
                    || tag.kind != "test"
                    || literal(tag.name).is_some())
        });
        let mut facts = Facts {
            import_ranges: Vec::new(),
            grammar,
            lines: source.lines().count().max(1),
            definitions: Vec::with_capacity(definitions.len() + sql_routines.len()),
            references: Vec::with_capacity(references.len()),
            imports: Vec::new(),
            routes: Vec::new(),
        };
        let mut def_spans = Vec::with_capacity(definitions.len() + sql_routines.len());
        for (id, definition) in definitions.iter().enumerate() {
            let RawDef {
                node,
                name_node,
                name,
                kind: raw_kind,
            } = definition;
            // Declarators that share one statement are siblings, not owners.
            let parent = (0..id).rev().find(|&parent| {
                let owner = definitions[parent].node;
                owner.end_byte() >= node.end_byte() && owner.byte_range() != node.byte_range()
            });
            let (name, kind) = match (grammar, *raw_kind) {
                (Grammar::Sql, kind) => (sql_name(name), kind),
                (Grammar::Hcl, "block") => (hcl_block_name(*node, name, source), *name),
                (Grammar::Hcl, kind) => (format!("local.{name}"), kind),
                (Grammar::Zig, "constant") => (name.to_string(), zig_container(*node)),
                // Dart spells the unnamed constructor `Type.new`.
                (Grammar::Dart, "constructor") if name_node.prev_named_sibling().is_none() => {
                    ("new".to_owned(), "constructor")
                }
                // PowerShell constructors are methods named after their class,
                // and callers reach them as `[Type]::new()`.
                (Grammar::PowerShell, "method")
                    if parent.is_some_and(|parent| definitions[parent].name == *name) =>
                {
                    ("new".to_owned(), "constructor")
                }
                (Grammar::PowerShell, "property") => {
                    (name.trim_start_matches('$').to_owned(), "property")
                }
                // `function global:Get-Item` defines `Get-Item` in the global scope.
                (Grammar::PowerShell, "function") => (
                    name.rsplit_once(':')
                        .map_or(*name, |(_, name)| name)
                        .to_owned(),
                    "function",
                ),
                (Grammar::Groovy, "task") => (literal(name).unwrap_or(name).to_owned(), "task"),
                // `new Type(…)` names the class, so the constructor takes
                // Dart's name for it rather than shadowing the class.
                (Grammar::Groovy | Grammar::CSharp, "constructor") => {
                    ("new".to_owned(), "constructor")
                }
                // Test calls and blocks are named by their quoted description,
                // which cannot collide with the function under test.
                (_, "test") => (format!("\"{}\"", literal(name).unwrap_or(name)), "test"),
                (_, kind) => (name.to_string(), kind),
            };
            let extension = matches!(
                grammar,
                Grammar::Swift | Grammar::Scala | Grammar::ObjectiveC
            )
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
                    Grammar::Java | Grammar::Kotlin | Grammar::Scala | Grammar::Groovy => {
                        callable && test_annotation(*node, source)
                    }
                    Grammar::CSharp => callable && csharp_test_attribute(*node, source),
                    Grammar::ObjectiveC => kind == "method" && name.starts_with("test"),
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
            } else if kind == "test"
                && matches!(
                    node.kind(),
                    "call_expression"
                        | "function_call"
                        | "call"
                        | "command"
                        | "macrocall_expression"
                )
            {
                // A test call's header is the line that names it.
                source[name_node.end_byte()..extent.end_byte()]
                    .find('\n')
                    .map(|offset| name_node.end_byte() + offset)
            } else {
                None
            };
            def_spans.push((node.start_byte(), node.end_byte()));
            facts.definitions.push(Definition {
                bytes: decorated_start(extent)..extent.end_byte(),
                signature_start: extent.start_byte(),
                header_end: header_end.unwrap_or_else(|| {
                    body.or_else(|| extent.child_by_field_name("body"))
                        .or_else(|| header_boundary(grammar, extent))
                        .map(|boundary| boundary.start_byte())
                        .or_else(|| value_initializer_end(*node, *name_node, extent, kind, source))
                        .unwrap_or(extent.end_byte())
                }),
                name,
                path,
                kind: kind.to_owned(),
                start: extent.start_position().row + 1,
                end: last_line(extent),
                declaration: name_node.start_position().row + 1,
                test,
            });
        }
        if !sql_routines.is_empty() {
            merge_sql_dollar_routines(&mut facts.definitions, &mut def_spans, &sql_routines);
        }
        // Deduplicate references in O(1) without cloning owned strings per lookup.
        let mut seen: HashSet<(u8, usize, Option<usize>, Cow<'_, str>)> =
            HashSet::with_capacity(references.len());
        // Where a call names the function it defines, as Elixir and Julia
        // heads do, the name is the definition, not a call to it.
        let mut declared = HashSet::with_capacity(definitions.len());
        for tag in &definitions {
            declared.insert((tag.name_node.start_byte(), tag.name_node.end_byte()));
        }
        for tag in references {
            if declared.contains(&(tag.name_node.start_byte(), tag.name_node.end_byte()))
                || (grammar == Grammar::Elixir && module_attribute(tag.node))
            {
                continue;
            }
            let kind = tag.kind;
            let name = if kind == Kind::Data {
                sql_name_cow(tag.name)
            } else {
                Cow::Borrowed(tag.name)
            };
            let line = tag.name_node.start_position().row + 1;
            let from = parent_symbol(&def_spans, tag.node.byte_range());
            let key = (kind as u8, line, from, name);
            if !seen.contains(&key) {
                let owned = key.3.as_ref().to_owned();
                seen.insert(key);
                facts.references.push(Reference {
                    name: owned,
                    from,
                    line,
                    kind,
                });
            }
        }
        for routine in &sql_routines {
            let from = parent_symbol(&def_spans, routine.full_range.clone());
            let body = &source[routine.body_content_range.clone()];
            for (table_name, line) in extract_sql_body_table_refs(body, routine.body_start_line) {
                let key = (Kind::Data as u8, line, from, Cow::Owned(table_name));
                if !seen.contains(&key) {
                    let owned = key.3.as_ref().to_owned();
                    seen.insert(key);
                    facts.references.push(Reference {
                        name: owned,
                        from,
                        line,
                        kind: Kind::Data,
                    });
                }
            }
        }
        let mut embedded_queries = Vec::new();
        let is_hcl = grammar == Grammar::Hcl;
        let is_sql = grammar == Grammar::Sql;
        let is_kotlin = grammar == Grammar::Kotlin;
        let has_routes = grammar == Grammar::Python || grammar.family() == Grammar::JavaScript;
        walk(tree.root_node(), |node| {
            let kind = node.kind();
            if matches!(
                kind,
                "import_statement"
                    | "import_from_statement"
                    | "import_declaration"
                    | "preproc_include"
                    | "using_directive"
                    | "namespace_use_declaration"
                    | "import_or_export"
            ) || (is_kotlin && kind == "import" && node.is_named())
                || import_like(grammar, kind, node, source)
            {
                facts.import_ranges.push(node.byte_range());
            }
            if is_hcl
                && kind == "variable_expr"
                && let Some(name) = hcl_reference(node, source)
            {
                let line = node.start_position().row + 1;
                let from = parent_symbol(&def_spans, node.byte_range());
                let key = (Kind::Reference as u8, line, from, Cow::Owned(name));
                if !seen.contains(&key) {
                    let owned = key.3.as_ref().to_owned();
                    seen.insert(key);
                    facts.references.push(Reference {
                        name: owned,
                        from,
                        line,
                        kind: Kind::Reference,
                    });
                }
            }
            if let Some(import) = module_import(grammar, kind, node, source) {
                facts.imports.push(import);
            }
            if has_routes
                && let Some((route, handler, argument)) = route_use(grammar, kind, node, source)
            {
                if let Some(handler) = handler {
                    if let Some(id) = parent_symbol(&def_spans, handler.byte_range()) {
                        facts.routes.push((route.to_owned(), id));
                    }
                } else {
                    facts.references.push(Reference {
                        name: route.to_owned(),
                        from: parent_symbol(&def_spans, node.byte_range()),
                        line: argument.start_position().row + 1,
                        kind: Kind::Route,
                    });
                }
            }
            if !is_sql && let Some(sql) = embedded_sql(kind, node, source) {
                embedded_queries.push((
                    sql,
                    parent_symbol(&def_spans, node.byte_range()),
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
        let mut count = 0usize;
        let mut matched = None;
        for child in node.named_children(&mut cursor) {
            if child.kind() == "property_element" {
                count += 1;
                if child.start_byte() <= name.start_byte() && name.end_byte() <= child.end_byte() {
                    matched = Some(child);
                }
            }
        }
        if count > 1
            && let Some(property) = matched
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
    match grammar {
        Grammar::Go => match node.kind() {
            "type_spec" => {
                let ty = node.child_by_field_name("type")?;
                let mut ty_cursor = ty.walk();
                match ty.kind() {
                    "struct_type" => ty
                        .named_children(&mut ty_cursor)
                        .find(|child| child.kind() == "field_declaration_list"),
                    "interface_type" => ty
                        .children(&mut ty_cursor)
                        .find(|child| child.kind() == "{"),
                    _ => None,
                }
            }
            _ => None,
        },
        Grammar::Elixir => node
            .named_children(&mut cursor)
            .find(|child| child.kind() == "do_block"),
        Grammar::PowerShell => node.children(&mut cursor).find(|child| child.kind() == "{"),
        Grammar::ObjectiveC => node.children(&mut cursor).find(|child| {
            matches!(
                child.kind(),
                "compound_statement"
                    | "method_declaration"
                    | "property_declaration"
                    | "implementation_definition"
                    | "instance_variables"
                    | "@end"
                    | "@optional"
                    | "@required"
            )
        }),
        // The signature, type head or module name comes first.
        Grammar::Julia => matches!(
            node.kind(),
            "function_definition"
                | "macro_definition"
                | "struct_definition"
                | "abstract_definition"
                | "module_definition"
        )
        .then(|| node.named_child(1))
        .flatten(),
        Grammar::Groovy => node.child_by_field_name("closure"),
        Grammar::CSharp if node.kind() == "property_declaration" => {
            node.child_by_field_name("accessors")
        }
        _ => header_block(grammar, node),
    }
}

fn header_block(grammar: Grammar, node: Node<'_>) -> Option<Node<'_>> {
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

/// For multi-line or long (>80-byte) value/type-alias declarations without a
/// block body, stop the outline header before the top-level `=` so large object,
/// array, or union initializers don't flood outlines while short single-line
/// constants and aliases keep their value.
fn value_initializer_end(
    node: Node<'_>,
    name_node: Node<'_>,
    extent: Node<'_>,
    kind: &str,
    source: &str,
) -> Option<usize> {
    if !matches!(
        kind,
        "type" | "constant" | "variable" | "property" | "field" | "local"
    ) {
        return None;
    }
    let fragment = source.get(extent.start_byte()..extent.end_byte())?;
    if !fragment.contains('\n') && fragment.len() <= 80 {
        return None;
    }
    let find_eq = |container: Node<'_>| {
        let mut cursor = container.walk();
        container
            .children(&mut cursor)
            .find(|child| child.kind() == "=" && child.start_byte() >= name_node.end_byte())
            .map(|eq| eq.start_byte())
    };
    let mut current = Some(name_node);
    while let Some(ancestor) = current {
        if let Some(pos) = find_eq(ancestor) {
            return Some(pos);
        }
        if ancestor == extent || ancestor == node {
            break;
        }
        current = ancestor.parent();
    }
    find_eq(node).or_else(|| (extent != node).then(|| find_eq(extent)).flatten())
}

// Swift extensions and Scala companion objects add members to a type declared
// beside them, as Rust impl blocks do. They are owners, not symbols, so a
// query for the type stays unambiguous.
fn type_extension(grammar: Grammar, node: Node<'_>, source: &str) -> Option<String> {
    let name = match (grammar, node.kind()) {
        // `@implementation Type`, a category `@interface Type (Name)` and a
        // class extension `@interface Type ()`.
        (Grammar::ObjectiveC, "class_implementation") => Some(text(node.named_child(0)?, source)),
        (Grammar::ObjectiveC, "class_interface") => {
            let mut cursor = node.walk();
            let extends = node.child_by_field_name("category").is_some()
                || node.children(&mut cursor).any(|child| child.kind() == "(");
            extends.then_some(text(node.named_child(0)?, source))
        }
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
fn is_test_annotation_node(annotation: Node<'_>, source: &str) -> bool {
    matches!(
        annotation.kind(),
        "annotation" | "marker_annotation" | "attribute"
    ) && text(annotation, source)
        .trim_start_matches('@')
        .split(|c: char| c == '(' || c.is_whitespace())
        .next()
        .and_then(|name| name.rsplit('.').next())
        .is_some_and(|name| name.ends_with("Test"))
}

fn test_annotation(node: Node<'_>, source: &str) -> bool {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "modifiers" {
            let mut mod_cursor = child.walk();
            if child
                .named_children(&mut mod_cursor)
                .any(|annotation| is_test_annotation_node(annotation, source))
            {
                return true;
            }
        } else if is_test_annotation_node(child, source) {
            return true;
        }
    }
    false
}

// xUnit (`[Fact]`, `[Theory]`), NUnit (`[Test]`, `[TestCase]`) and MSTest
// (`[TestMethod]`, `[DataTestMethod]`) attributes on C# methods.
fn csharp_test_attribute(node: Node<'_>, source: &str) -> bool {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "attribute_list" {
            continue;
        }
        let mut attr_cursor = child.walk();
        for attr in child.named_children(&mut attr_cursor) {
            if attr.kind() != "attribute" {
                continue;
            }
            let Some(name_node) = attr.child_by_field_name("name") else {
                continue;
            };
            let raw = text(name_node, source);
            let short = raw
                .rsplit(['.', ':'])
                .next()
                .unwrap_or(raw)
                .split('<')
                .next()
                .unwrap_or(raw);
            let short = short.strip_suffix("Attribute").unwrap_or(short);
            if matches!(
                short,
                "Fact"
                    | "Theory"
                    | "Test"
                    | "TestMethod"
                    | "TestCase"
                    | "TestCaseSource"
                    | "DataTestMethod"
            ) {
                return true;
            }
        }
    }
    false
}

// Terraform addresses: `aws_s3_bucket.logs`, `data.T.N`, `var.x`, `module.y`.
// Other top-level blocks join their type and labels the same way.
fn hcl_block_name(node: Node<'_>, kind: &str, source: &str) -> String {
    let mut out = String::new();
    match kind {
        "resource" => {}
        "variable" => out.push_str("var"),
        other => out.push_str(other),
    }
    let mut cursor = node.walk();
    for child in node
        .named_children(&mut cursor)
        .skip(1)
        .take_while(|child| matches!(child.kind(), "string_lit" | "identifier"))
    {
        let label = text(child, source);
        let label = label
            .strip_prefix('"')
            .and_then(|label| label.strip_suffix('"'))
            .unwrap_or(label);
        if !out.is_empty() {
            out.push('.');
        }
        out.push_str(label);
    }
    if out.is_empty() {
        out.push_str(kind);
    }
    out
}

fn hcl_reference(node: Node<'_>, source: &str) -> Option<String> {
    let root = text(node, source);
    let width = match root {
        "count" | "each" | "self" | "path" | "terraform" => return None,
        "data" | "ephemeral" => 3,
        _ => 2,
    };
    let mut parts = [root, "", ""];
    let mut count = 1usize;
    let mut next = node.next_named_sibling();
    while count < width {
        while let Some(candidate) = next
            && candidate.kind() == "comment"
        {
            next = candidate.next_named_sibling();
        }
        let Some(attribute) = next.filter(|next| next.kind() == "get_attr") else {
            break;
        };
        parts[count] = text(attribute.named_child(0)?, source);
        count += 1;
        next = attribute.next_named_sibling();
    }
    if count != width {
        return None;
    }
    let total_len: usize = parts[..count].iter().map(|p| p.len()).sum::<usize>() + (count - 1);
    let mut out = String::with_capacity(total_len);
    for (idx, part) in parts[..count].iter().enumerate() {
        if idx > 0 {
            out.push('.');
        }
        out.push_str(part);
    }
    Some(out)
}

fn hcl_module_source<'a>(node: Node<'_>, source: &'a str) -> Option<(&'a str, usize)> {
    let mut cursor = node.walk();
    let mut children = node.named_children(&mut cursor);
    let kind_node = children.next()?;
    if kind_node.kind() != "identifier" || text(kind_node, source) != "module" {
        return None;
    }
    let body = children.find(|child| child.kind() == "body")?;
    let mut body_cursor = body.walk();
    for attr in body
        .named_children(&mut body_cursor)
        .filter(|child| child.kind() == "attribute")
    {
        let key = attr.named_child(0)?;
        if key.kind() != "identifier" || text(key, source) != "source" {
            continue;
        }
        let expr = attr.named_child(1)?;
        let raw = text(expr, source).trim();
        let inner = raw.strip_prefix('"')?.strip_suffix('"')?;
        if (inner.starts_with("./") || inner.starts_with("../"))
            && !inner.contains("${")
            && !inner.contains("%{")
        {
            return Some((inner, attr.start_position().row + 1));
        }
    }
    None
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

/// Whether an Elixir node sits in a module attribute such as `@spec` or
/// `@doc`, whose calls are type and documentation syntax.
fn module_attribute(node: Node<'_>) -> bool {
    std::iter::successors(Some(node), Node::parent).any(|ancestor| {
        ancestor.kind() == "unary_operator"
            && ancestor
                .child_by_field_name("operator")
                .is_some_and(|operator| operator.kind() == "@")
    })
}

/// Import statements that a grammar spells as ordinary calls or commands, so
/// outlines can show them as imports.
fn import_like(grammar: Grammar, kind: &str, node: Node<'_>, source: &str) -> bool {
    match (grammar, kind) {
        (
            Grammar::JavaScript
            | Grammar::TypeScript
            | Grammar::Tsx
            | Grammar::Vue
            | Grammar::Svelte
            | Grammar::Astro,
            "export_statement",
        ) => node.child_by_field_name("source").is_some(),
        (Grammar::Elixir, "call") => node.child_by_field_name("target").is_some_and(|target| {
            matches!(text(target, source), "alias" | "import" | "require" | "use")
        }),
        (Grammar::PowerShell, "command") => {
            node.child_by_field_name("command_name")
                .is_some_and(|name| {
                    let name = text(name, source);
                    name.eq_ignore_ascii_case("using") || name.eq_ignore_ascii_case("Import-Module")
                })
        }
        (Grammar::Julia, "using_statement") | (Grammar::ObjectiveC, "module_import") => true,
        _ => false,
    }
}

/// Whether C-family source is Objective-C: a header or source that declares a
/// class or protocol, or imports with `#import` or `@import`.
fn objective_c(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim_start();
        [
            "@interface",
            "@implementation",
            "@protocol",
            "#import",
            "@import",
        ]
        .iter()
        .any(|marker| line.starts_with(marker))
    })
}

/// Objective-C headers wrap declarations in lines that are a bare macro, such
/// as `NS_ASSUME_NONNULL_BEGIN`, which the grammar cannot place. Spaces keep
/// every offset.
fn blank_bare_macros(source: &str) -> String {
    let mut blanked = String::with_capacity(source.len());
    for line in source.split_inclusive('\n') {
        let code = line.trim();
        let (name, arguments) = code.split_at(code.find('(').unwrap_or(code.len()));
        let bare = name.contains('_')
            && name.starts_with(|c: char| c.is_ascii_uppercase())
            && name
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
            && (arguments.is_empty() || (arguments.ends_with(')') && !arguments.contains(';')));
        if bare {
            blanked.extend(
                line.chars()
                    .map(|c| if c == '\n' || c == '\r' { c } else { ' ' }),
            );
        } else {
            blanked.push_str(line);
        }
    }
    blanked
}

fn last_line(node: Node<'_>) -> usize {
    let end = node.end_position();
    (end.row + usize::from(end.column != 0)).max(node.start_position().row + 1)
}

fn parent_symbol(spans: &[(usize, usize)], range: std::ops::Range<usize>) -> Option<usize> {
    let end = spans.partition_point(|&(start, _)| start <= range.start);
    (0..end).rev().find(|&id| spans[id].1 >= range.end)
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

fn sql_name_cow(value: &str) -> Cow<'_, str> {
    if value
        .bytes()
        .all(|b| !b.is_ascii_uppercase() && !b.is_ascii_whitespace() && b != b'"' && b != b'`')
    {
        return Cow::Borrowed(value);
    }
    let mut out = String::with_capacity(value.len());
    for (idx, part) in value.split('.').enumerate() {
        if idx > 0 {
            out.push('.');
        }
        let cleaned = part.trim().trim_matches(['"', '\u{60}']);
        for ch in cleaned.chars() {
            out.push(ch.to_ascii_lowercase());
        }
    }
    Cow::Owned(out)
}

fn sql_name(value: &str) -> String {
    sql_name_cow(value).into_owned()
}

fn embedded_sql<'a>(kind: &str, node: Node<'_>, source: &'a str) -> Option<&'a str> {
    if !matches!(
        kind,
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
    let first = body.split_whitespace().next()?;
    (first.eq_ignore_ascii_case("SELECT")
        || first.eq_ignore_ascii_case("WITH")
        || first.eq_ignore_ascii_case("INSERT")
        || first.eq_ignore_ascii_case("UPDATE")
        || first.eq_ignore_ascii_case("DELETE"))
    .then_some(body)
}

fn route_use<'a, 'tree>(
    grammar: Grammar,
    kind: &str,
    node: Node<'tree>,
    source: &'a str,
) -> Option<(&'a str, Option<Node<'tree>>, Node<'tree>)> {
    let python = grammar == Grammar::Python && kind == "decorator";
    let javascript = grammar.family() == Grammar::JavaScript && kind == "call_expression";
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

const JS_TS_EXTENSIONS: &[&str] = &[".ts", ".tsx", ".js", ".jsx", ".mts", ".cts", ".mjs", ".cjs"];

fn strip_js_ts_extension(path: &str) -> &str {
    for &ext in JS_TS_EXTENSIONS {
        if let Some(stem) = path.strip_suffix(ext) {
            return stem;
        }
    }
    path
}

fn strip_js_query_and_hash(spec: &str) -> &str {
    let spec = spec.split_once('?').map_or(spec, |(head, _)| head);
    // Preserve leading `#` for Node subpath imports (`#utils/foo`).
    match spec.find('#') {
        Some(pos) if pos > 0 => &spec[..pos],
        _ => spec,
    }
}

fn collapse_js_slashes(spec: &str) -> String {
    let mut out = String::with_capacity(spec.len());
    let mut prev_slash = false;
    for ch in spec.chars() {
        let ch = if ch == '\\' { '/' } else { ch };
        if ch == '/' {
            if !prev_slash {
                out.push('/');
                prev_slash = true;
            }
        } else {
            out.push(ch);
            prev_slash = false;
        }
    }
    out
}

/// Normalizes a JS/TS/TSX module specifier into a clean target path for
/// `Import.path`: strips query/hash fragments, collapses redundant `./` and
/// duplicate slashes, trims trailing slashes, and collapses barrel `/index`
/// (or `/index.<js/ts ext>`) suffixes while keeping leading `./` or `../` on
/// relative specifiers so `index.rs::import_target` resolves them directly.
fn normalize_js_import_path(raw: &str) -> Option<String> {
    let trimmed = strip_js_query_and_hash(raw.trim()).trim();
    if trimmed.is_empty() {
        return None;
    }
    let collapsed = collapse_js_slashes(trimmed);
    let mut normalized = collapsed.as_str();
    if normalized.len() > 1 {
        normalized = normalized.trim_end_matches('/');
    }
    if normalized.is_empty() {
        return None;
    }
    // Collapse redundant `./` segments: `././foo` -> `./foo`, `./foo/./bar` -> `./foo/bar`.
    let relative_dot = normalized.starts_with("./");
    let mut segments = Vec::new();
    for seg in normalized.split('/') {
        if seg == "." && !segments.is_empty() {
            continue;
        }
        segments.push(seg);
    }
    let mut cleaned = if relative_dot {
        let mut s = String::with_capacity(normalized.len());
        s.push('.');
        for seg in segments.into_iter().skip_while(|&seg| seg == ".") {
            s.push('/');
            s.push_str(seg);
        }
        s
    } else {
        segments.join("/")
    };
    // Normalize barrel `foo/index` and `foo/index.<ext>` to `foo` when the
    // parent stem has no dot (so `Path::extension` in `import_target` is not
    // fooled by a dotted directory name like `foo.v2`).
    if let Some((parent, tail)) = cleaned.rsplit_once('/')
        && strip_js_ts_extension(tail) == "index"
        && !matches!(parent, "" | "." | ".." | "@" | "~" | "#")
    {
        let last_dir = parent.rsplit('/').next().unwrap_or(parent);
        if !last_dir.contains('.') {
            cleaned.truncate(parent.len());
        }
    }
    (!cleaned.is_empty()).then_some(cleaned)
}

/// Produces a normalized module stem from a JS/TS/TSX specifier: strips
/// leading `./` or path-alias prefixes (`@/`, `~/`, `#/`), collapses barrel
/// `/index` segments, and drops JS/TS file extensions.
pub fn normalize_js_module_stem(specifier: &str) -> String {
    let base = normalize_js_import_path(specifier).unwrap_or_else(|| specifier.trim().to_owned());
    let mut stem = base.as_str();
    while let Some(rest) = stem.strip_prefix("./") {
        stem = rest;
    }
    for alias_prefix in ["@/", "~/", "#/"] {
        if let Some(rest) = stem.strip_prefix(alias_prefix) {
            stem = rest;
            break;
        }
    }
    stem = strip_js_ts_extension(stem);
    if let Some(parent) = stem.strip_suffix("/index")
        && !parent.is_empty()
        && parent != ".."
    {
        stem = parent;
    }
    stem.to_owned()
}

fn static_js_specifier<'a>(node: Node<'_>, source: &'a str) -> Option<&'a str> {
    let raw = text(node, source);
    match node.kind() {
        "string" => literal(raw),
        "template_string" if !raw.contains("${") => literal(raw),
        _ => None,
    }
}

fn collect_js_import_bindings(
    node: Node<'_>,
    source: &str,
    bindings: &mut BTreeMap<String, String>,
) {
    walk(node, |child| match child.kind() {
        "import_specifier" => {
            if let Some(name_node) = child.child_by_field_name("name") {
                let alias_node = child.child_by_field_name("alias").unwrap_or(name_node);
                let name_raw = text(name_node, source);
                let alias_raw = text(alias_node, source);
                let name = literal(name_raw).unwrap_or(name_raw);
                let alias = literal(alias_raw).unwrap_or(alias_raw);
                if !name.is_empty() && !alias.is_empty() {
                    bindings.insert(alias.to_owned(), name.to_owned());
                }
            }
        }
        "import_clause" => {
            let mut cursor = child.walk();
            for clause_child in child.named_children(&mut cursor) {
                if clause_child.kind() == "identifier" {
                    let default_name = text(clause_child, source);
                    if !default_name.is_empty() {
                        bindings
                            .entry(default_name.to_owned())
                            .or_insert_with(|| default_name.to_owned());
                    }
                }
            }
        }
        _ => {}
    });
}

fn collect_js_export_bindings(
    node: Node<'_>,
    source: &str,
    bindings: &mut BTreeMap<String, String>,
) {
    walk(node, |child| {
        if child.kind() == "export_specifier"
            && let Some(name_node) = child.child_by_field_name("name")
        {
            let alias_node = child.child_by_field_name("alias").unwrap_or(name_node);
            let name_raw = text(name_node, source);
            let alias_raw = text(alias_node, source);
            let name = literal(name_raw).unwrap_or(name_raw);
            let alias = literal(alias_raw).unwrap_or(alias_raw);
            if !name.is_empty() && !alias.is_empty() {
                bindings.insert(alias.to_owned(), name.to_owned());
            }
        }
    });
}

fn collect_cjs_require_bindings(
    call_node: Node<'_>,
    source: &str,
    bindings: &mut BTreeMap<String, String>,
) {
    let Some(parent) = call_node.parent() else {
        return;
    };
    if parent.kind() != "variable_declarator"
        || parent.child_by_field_name("value") != Some(call_node)
    {
        return;
    }
    let Some(lhs) = parent.child_by_field_name("name") else {
        return;
    };
    if lhs.kind() != "object_pattern" {
        return;
    }
    let mut cursor = lhs.walk();
    for item in lhs.named_children(&mut cursor) {
        match item.kind() {
            "shorthand_property_identifier_pattern" => {
                let name = text(item, source);
                if !name.is_empty() {
                    bindings.insert(name.to_owned(), name.to_owned());
                }
            }
            "pair_pattern" => {
                if let Some(key) = item.child_by_field_name("key")
                    && let Some(value) = item.child_by_field_name("value")
                    && value.kind() == "identifier"
                {
                    let key_raw = text(key, source);
                    let key_name = literal(key_raw).unwrap_or(key_raw);
                    let alias = text(value, source);
                    if !key_name.is_empty() && !alias.is_empty() {
                        bindings.insert(alias.to_owned(), key_name.to_owned());
                    }
                }
            }
            _ => {}
        }
    }
}

fn module_import(grammar: Grammar, kind: &str, node: Node<'_>, source: &str) -> Option<Import> {
    let mut bindings = BTreeMap::new();
    let path: Cow<'_, str> = if grammar.family() == Grammar::JavaScript {
        match kind {
            "import_statement" => {
                let source_node = node.child_by_field_name("source").or_else(|| {
                    // TypeScript `import foo = require('./foo')`.
                    let mut cursor = node.walk();
                    node.named_children(&mut cursor)
                        .find(|child| child.kind() == "import_require_clause")
                        .and_then(|clause| clause.child_by_field_name("source"))
                })?;
                let raw = static_js_specifier(source_node, source)?;
                collect_js_import_bindings(node, source, &mut bindings);
                Cow::Owned(normalize_js_import_path(raw)?)
            }
            "export_statement" => {
                let source_node = node.child_by_field_name("source")?;
                let raw = static_js_specifier(source_node, source)?;
                collect_js_export_bindings(node, source, &mut bindings);
                Cow::Owned(normalize_js_import_path(raw)?)
            }
            "call_expression" => {
                let func = node.child_by_field_name("function")?;
                let is_dynamic_import = func.kind() == "import";
                let is_require = !is_dynamic_import
                    && func.kind() == "identifier"
                    && text(func, source) == "require";
                if !is_dynamic_import && !is_require {
                    return None;
                }
                let arg = node.child_by_field_name("arguments")?.named_child(0)?;
                let raw = static_js_specifier(arg, source)?;
                if is_require {
                    collect_cjs_require_bindings(node, source, &mut bindings);
                }
                Cow::Owned(normalize_js_import_path(raw)?)
            }
            _ => return None,
        }
    } else if grammar == Grammar::Python && kind == "import_from_statement" {
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
        Cow::Borrowed(text(module, source))
    } else if matches!(grammar, Grammar::C | Grammar::Cpp | Grammar::ObjectiveC)
        && kind == "preproc_include"
    {
        Cow::Borrowed(literal(text(node.child_by_field_name("path")?, source))?)
    } else if grammar == Grammar::Julia
        && kind == "call_expression"
        && text(node.named_child(0)?, source) == "include"
    {
        let argument = node.named_child(1)?.named_child(0)?;
        Cow::Borrowed(
            (argument.kind() == "string_literal").then(|| literal(text(argument, source)))??,
        )
    } else if grammar == Grammar::Hcl && kind == "block" {
        let (path, line) = hcl_module_source(node, source)?;
        return Some(Import {
            path: path.to_owned(),
            bindings,
            line,
        });
    } else {
        return None;
    };
    Some(Import {
        path: path.into_owned(),
        bindings,
        line: node.start_position().row + 1,
    })
}

// ---------------------------------------------------------------------------
// PostgreSQL dollar-quoted FUNCTION / PROCEDURE support
// ---------------------------------------------------------------------------

struct SqlDollarRoutine {
    kind: &'static str,
    name: String,
    full_range: std::ops::Range<usize>,
    name_range: std::ops::Range<usize>,
    header_end: usize,
    body_content_range: std::ops::Range<usize>,
    start_line: usize,
    end_line: usize,
    decl_line: usize,
    body_start_line: usize,
}

fn line_at_offset(source: &str, offset: usize) -> usize {
    source
        .as_bytes()
        .get(..offset)
        .unwrap_or(source.as_bytes())
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
        + 1
}

/// Parses a PostgreSQL dollar-quote delimiter starting at `pos` (`$$` or
/// `$tag$` where `tag` matches `[A-Za-z_][A-Za-z0-9_]*`). Positional params
/// like `$1` are rejected because tags cannot begin with an ASCII digit.
fn parse_dollar_tag(bytes: &[u8], pos: usize) -> Option<&[u8]> {
    if bytes.get(pos) != Some(&b'$') {
        return None;
    }
    let mut i = pos + 1;
    if bytes.get(i) == Some(&b'$') {
        return Some(&bytes[pos..=i]);
    }
    let first = *bytes.get(i)?;
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return None;
    }
    i += 1;
    while let Some(&b) = bytes.get(i) {
        if b == b'$' {
            return Some(&bytes[pos..=i]);
        }
        if !(b.is_ascii_alphanumeric() || b == b'_') {
            return None;
        }
        i += 1;
    }
    None
}

fn skip_sql_ws_and_comments(bytes: &[u8], mut i: usize) -> usize {
    let len = bytes.len();
    while i < len {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
        } else if b == b'-' && bytes.get(i + 1) == Some(&b'-') {
            i += 2;
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
        } else if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            let mut depth = 1usize;
            while i < len && depth > 0 {
                if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        } else {
            break;
        }
    }
    i
}

fn skip_sql_single_quoted(bytes: &[u8], start: usize, escape_backslash: bool) -> usize {
    let len = bytes.len();
    let mut i = start + 1;
    while i < len {
        let b = bytes[i];
        if escape_backslash && b == b'\\' {
            i = (i + 2).min(len);
        } else if b == b'\'' {
            if bytes.get(i + 1) == Some(&b'\'') {
                i += 2;
            } else {
                return i + 1;
            }
        } else {
            i += 1;
        }
    }
    len
}

fn skip_sql_quoted_ident(bytes: &[u8], start: usize, quote: u8) -> usize {
    let len = bytes.len();
    let mut i = start + 1;
    while i < len {
        if bytes[i] == quote {
            if bytes.get(i + 1) == Some(&quote) {
                i += 2;
            } else {
                return i + 1;
            }
        } else {
            i += 1;
        }
    }
    len
}

fn read_sql_word(source: &str, pos: usize) -> Option<(&str, usize)> {
    let bytes = source.as_bytes();
    let first = *bytes.get(pos)?;
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return None;
    }
    let mut end = pos + 1;
    while let Some(&b) = bytes.get(end) {
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'$' {
            end += 1;
        } else {
            break;
        }
    }
    Some((&source[pos..end], end))
}

fn read_sql_ident_part(source: &str, pos: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    match *bytes.get(pos)? {
        b'"' => Some(skip_sql_quoted_ident(bytes, pos, b'"')),
        b'`' => Some(skip_sql_quoted_ident(bytes, pos, b'`')),
        b if b.is_ascii_alphabetic() || b == b'_' => read_sql_word(source, pos).map(|(_, end)| end),
        _ => None,
    }
}

fn read_sql_object_reference(
    source: &str,
    start: usize,
) -> Option<(std::ops::Range<usize>, usize)> {
    let bytes = source.as_bytes();
    let mut end = read_sql_ident_part(source, start)?;
    loop {
        let dot = skip_sql_ws_and_comments(bytes, end);
        if bytes.get(dot) != Some(&b'.') {
            break;
        }
        let next = skip_sql_ws_and_comments(bytes, dot + 1);
        let Some(next_end) = read_sql_ident_part(source, next) else {
            break;
        };
        end = next_end;
    }
    Some((start..end, end))
}

fn is_sql_top_level_starter(word: &str) -> bool {
    word.eq_ignore_ascii_case("CREATE")
        || word.eq_ignore_ascii_case("ALTER")
        || word.eq_ignore_ascii_case("DROP")
        || word.eq_ignore_ascii_case("GRANT")
        || word.eq_ignore_ascii_case("REVOKE")
        || word.eq_ignore_ascii_case("SELECT")
        || word.eq_ignore_ascii_case("INSERT")
        || word.eq_ignore_ascii_case("UPDATE")
        || word.eq_ignore_ascii_case("DELETE")
        || word.eq_ignore_ascii_case("BEGIN")
        || word.eq_ignore_ascii_case("COMMIT")
        || word.eq_ignore_ascii_case("COMMENT")
}

/// Scans PostgreSQL `CREATE [OR REPLACE] FUNCTION/PROCEDURE ... AS $tag$ ... $tag$`
/// routines so their symbol spans cover the complete dollar-quoted body and
/// trailing clauses (never cutting off at internal semicolons).
fn scan_sql_dollar_routines(source: &str) -> Vec<SqlDollarRoutine> {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut routines = Vec::new();
    let mut i = 0usize;
    while i < len {
        i = skip_sql_ws_and_comments(bytes, i);
        if i >= len {
            break;
        }
        let b = bytes[i];
        if b == b'\'' {
            i = skip_sql_single_quoted(bytes, i, false);
            continue;
        }
        if matches!(b, b'e' | b'E') && bytes.get(i + 1) == Some(&b'\'') {
            i = skip_sql_single_quoted(bytes, i + 1, true);
            continue;
        }
        if b == b'"' || b == b'`' {
            i = skip_sql_quoted_ident(bytes, i, b);
            continue;
        }
        if b == b'$'
            && let Some(tag_bytes) = parse_dollar_tag(bytes, i)
            && let Ok(tag) = std::str::from_utf8(tag_bytes)
        {
            let body_start = i + tag.len();
            i = source[body_start..]
                .find(tag)
                .map_or(len, |rel| body_start + rel + tag.len());
            continue;
        }
        let Some((word, next)) = read_sql_word(source, i) else {
            i += source[i..].chars().next().map_or(1, char::len_utf8);
            continue;
        };
        if !word.eq_ignore_ascii_case("CREATE") {
            i = next;
            continue;
        }
        let create_start = i;
        let mut cur = skip_sql_ws_and_comments(bytes, next);
        if let Some((w, after_or)) = read_sql_word(source, cur)
            && w.eq_ignore_ascii_case("OR")
        {
            let after_ws = skip_sql_ws_and_comments(bytes, after_or);
            if let Some((w2, after_replace)) = read_sql_word(source, after_ws)
                && w2.eq_ignore_ascii_case("REPLACE")
            {
                cur = skip_sql_ws_and_comments(bytes, after_replace);
            }
        }
        let Some((kind_word, after_kind)) = read_sql_word(source, cur) else {
            i = next;
            continue;
        };
        let kind = if kind_word.eq_ignore_ascii_case("FUNCTION") {
            "function"
        } else if kind_word.eq_ignore_ascii_case("PROCEDURE") {
            "procedure"
        } else {
            i = after_kind;
            continue;
        };
        cur = skip_sql_ws_and_comments(bytes, after_kind);
        // Optional `IF NOT EXISTS`.
        if let Some((w, after_if)) = read_sql_word(source, cur)
            && w.eq_ignore_ascii_case("IF")
        {
            let p1 = skip_sql_ws_and_comments(bytes, after_if);
            if let Some((w2, after_not)) = read_sql_word(source, p1)
                && w2.eq_ignore_ascii_case("NOT")
            {
                let p2 = skip_sql_ws_and_comments(bytes, after_not);
                if let Some((w3, after_exists)) = read_sql_word(source, p2)
                    && w3.eq_ignore_ascii_case("EXISTS")
                {
                    cur = skip_sql_ws_and_comments(bytes, after_exists);
                }
            }
        }
        let Some((name_range, after_name)) = read_sql_object_reference(source, cur) else {
            i = after_kind;
            continue;
        };
        // Scan header until a top-level dollar-quoted body or statement end.
        let mut scan = after_name;
        let mut paren_depth = 0usize;
        let mut found_body = None;
        while scan < len {
            scan = skip_sql_ws_and_comments(bytes, scan);
            if scan >= len {
                break;
            }
            let ch = bytes[scan];
            match ch {
                b'(' => {
                    paren_depth += 1;
                    scan += 1;
                }
                b')' => {
                    paren_depth = paren_depth.saturating_sub(1);
                    scan += 1;
                }
                b';' if paren_depth == 0 => break,
                b'\'' => {
                    scan = skip_sql_single_quoted(bytes, scan, false);
                }
                b'e' | b'E' if bytes.get(scan + 1) == Some(&b'\'') => {
                    scan = skip_sql_single_quoted(bytes, scan + 1, true);
                }
                b'"' | b'`' => {
                    scan = skip_sql_quoted_ident(bytes, scan, ch);
                }
                b'$' if paren_depth == 0 => {
                    if let Some(tag_bytes) = parse_dollar_tag(bytes, scan)
                        && let Ok(tag) = std::str::from_utf8(tag_bytes)
                    {
                        let body_start = scan + tag.len();
                        if let Some(rel_close) = source[body_start..].find(tag) {
                            let body_end = body_start + rel_close;
                            let close_end = body_end + tag.len();
                            found_body = Some((scan, body_start, body_end, close_end));
                        }
                        break;
                    }
                    scan += 1;
                }
                _ => {
                    if let Some((_, w_end)) = read_sql_word(source, scan) {
                        scan = w_end;
                    } else {
                        scan += source[scan..].chars().next().map_or(1, char::len_utf8);
                    }
                }
            }
        }
        let Some((open_tag_start, body_start, body_end, close_end)) = found_body else {
            i = after_name;
            continue;
        };
        // Scan trailing routine attributes (e.g. `LANGUAGE plpgsql`, `STABLE`,
        // `SECURITY DEFINER`, `SET search_path = ...`) up to `;`.
        let mut tail = close_end;
        let mut last_end = close_end;
        let mut tail_parens = 0usize;
        while tail < len {
            tail = skip_sql_ws_and_comments(bytes, tail);
            if tail >= len {
                break;
            }
            let ch = bytes[tail];
            if ch == b';' && tail_parens == 0 {
                last_end = tail + 1;
                break;
            }
            if ch == b'(' {
                tail_parens += 1;
                tail += 1;
                last_end = tail;
            } else if ch == b')' {
                tail_parens = tail_parens.saturating_sub(1);
                tail += 1;
                last_end = tail;
            } else if ch == b'\'' {
                tail = skip_sql_single_quoted(bytes, tail, false);
                last_end = tail;
            } else if ch == b'"' || ch == b'`' {
                tail = skip_sql_quoted_ident(bytes, tail, ch);
                last_end = tail;
            } else if let Some((w, w_end)) = read_sql_word(source, tail) {
                if tail_parens == 0 && is_sql_top_level_starter(w) {
                    break;
                }
                tail = w_end;
                last_end = w_end;
            } else {
                tail += source[tail..].chars().next().map_or(1, char::len_utf8);
                last_end = tail;
            }
        }
        let header_end = create_start + source[create_start..open_tag_start].trim_end().len();
        let name = sql_name(&source[name_range.clone()]);
        let start_line = line_at_offset(source, create_start);
        let end_line = line_at_offset(source, last_end.saturating_sub(1));
        let decl_line = line_at_offset(source, name_range.start);
        let body_start_line = line_at_offset(source, body_start);
        routines.push(SqlDollarRoutine {
            kind,
            name,
            full_range: create_start..last_end,
            name_range,
            header_end,
            body_content_range: body_start..body_end,
            start_line,
            end_line,
            decl_line,
            body_start_line,
        });
        i = last_end;
    }
    routines
}

/// Replaces each dollar-quoted routine's interior with equal-byte-length
/// `SELECT 1` + spaces (preserving every `\n` and `\r`) so `tree-sitter-sequel`
/// parses surrounding DDL without getting stuck on PL/pgSQL syntax.
fn mask_sql_dollar_bodies(source: &str, routines: &[SqlDollarRoutine]) -> Option<String> {
    let mut buf = source.as_bytes().to_vec();
    let mut modified = false;
    for routine in routines {
        let range = routine.body_content_range.clone();
        if range.len() < 8 {
            continue;
        }
        let slice = &mut buf[range];
        // Find a contiguous non-newline span of >= 8 bytes for `SELECT 1`.
        let mut placed = false;
        let mut run_start = 0usize;
        for idx in 0..=slice.len() {
            let is_nl = idx == slice.len() || matches!(slice[idx], b'\n' | b'\r');
            if is_nl {
                if !placed && idx - run_start >= 8 {
                    slice[run_start..run_start + 8].copy_from_slice(b"SELECT 1");
                    for b in &mut slice[run_start + 8..idx] {
                        *b = b' ';
                    }
                    placed = true;
                } else {
                    for b in &mut slice[run_start..idx] {
                        *b = b' ';
                    }
                }
                run_start = idx + 1;
            }
        }
        if !placed {
            // Fallback: place `SELECT` and `1` across separate short lines.
            let mut s_placed = false;
            let mut one_placed = false;
            let mut r_start = 0usize;
            for idx in 0..=slice.len() {
                let is_nl = idx == slice.len() || matches!(slice[idx], b'\n' | b'\r');
                if is_nl {
                    let span = idx - r_start;
                    if !s_placed && span >= 6 {
                        slice[r_start..r_start + 6].copy_from_slice(b"SELECT");
                        s_placed = true;
                    } else if s_placed && !one_placed && span >= 1 {
                        slice[r_start] = b'1';
                        one_placed = true;
                    }
                    r_start = idx + 1;
                }
            }
            if !(s_placed && one_placed) {
                // Restore original bytes for this routine if too narrow.
                buf[routine.body_content_range.clone()]
                    .copy_from_slice(&source.as_bytes()[routine.body_content_range.clone()]);
                continue;
            }
        }
        modified = true;
    }
    modified.then(|| String::from_utf8(buf).expect("ASCII masking preserves UTF-8"))
}

fn merge_sql_dollar_routines(
    definitions: &mut Vec<Definition>,
    spans: &mut Vec<(usize, usize)>,
    routines: &[SqlDollarRoutine],
) {
    // Drop any broken/partial Tree-sitter definitions strictly nested inside a
    // dollar-quoted routine body.
    let mut keep = vec![true; definitions.len()];
    for (idx, span) in spans.iter().enumerate() {
        for routine in routines {
            if span.0 > routine.full_range.start && span.1 <= routine.full_range.end {
                keep[idx] = false;
                break;
            }
        }
    }
    if keep.iter().any(|&k| !k) {
        let mut idx = 0;
        definitions.retain(|_| {
            let k = keep[idx];
            idx += 1;
            k
        });
        let mut idx = 0;
        spans.retain(|_| {
            let k = keep[idx];
            idx += 1;
            k
        });
    }
    let mut inserted = false;
    for routine in routines {
        let existing = spans.iter().position(|&(start, end)| {
            start == routine.full_range.start
                || (start <= routine.name_range.start && end >= routine.name_range.end)
        });
        if let Some(idx) = existing {
            let def = &mut definitions[idx];
            def.bytes = routine.full_range.clone();
            def.signature_start = routine.full_range.start;
            def.header_end = routine.header_end;
            def.start = routine.start_line;
            def.end = routine.end_line;
            def.declaration = routine.decl_line;
            spans[idx] = (routine.full_range.start, routine.full_range.end);
        } else {
            definitions.push(Definition {
                bytes: routine.full_range.clone(),
                signature_start: routine.full_range.start,
                header_end: routine.header_end,
                name: routine.name.clone(),
                path: routine.name.clone(),
                kind: routine.kind.to_owned(),
                start: routine.start_line,
                end: routine.end_line,
                declaration: routine.decl_line,
                test: false,
            });
            spans.push((routine.full_range.start, routine.full_range.end));
            inserted = true;
        }
    }
    if inserted {
        let mut order: Vec<usize> = (0..definitions.len()).collect();
        order.sort_by_key(|&i| (spans[i].0, std::cmp::Reverse(spans[i].1)));
        let old_defs = std::mem::take(definitions);
        let old_spans = std::mem::take(spans);
        let mut slots: Vec<Option<Definition>> = old_defs.into_iter().map(Some).collect();
        for i in order {
            definitions.push(slots[i].take().unwrap());
            spans.push(old_spans[i]);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SqlTokKind {
    Word,
    Quoted,
    Dot,
    LParen,
    RParen,
    Comma,
    Semi,
    Other,
}

struct SqlTok<'a> {
    kind: SqlTokKind,
    text: &'a str,
    line_offset: usize,
}

fn tokenize_sql_body(body: &str) -> Vec<SqlTok<'_>> {
    let bytes = body.as_bytes();
    let len = bytes.len();
    let mut tokens = Vec::new();
    let mut i = 0usize;
    let mut line_offset = 0usize;
    let count_newlines = |slice: &[u8]| slice.iter().filter(|&&b| b == b'\n').count();
    while i < len {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            if b == b'\n' {
                line_offset += 1;
            }
            i += 1;
            continue;
        }
        if b == b'-' && bytes.get(i + 1) == Some(&b'-') {
            i += 2;
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            let start = i;
            i = skip_sql_ws_and_comments(bytes, i);
            line_offset += count_newlines(&bytes[start..i]);
            continue;
        }
        if b == b'\'' {
            let start = i;
            i = skip_sql_single_quoted(bytes, i, false);
            line_offset += count_newlines(&bytes[start..i]);
            tokens.push(SqlTok {
                kind: SqlTokKind::Other,
                text: &body[start..i],
                line_offset,
            });
            continue;
        }
        if matches!(b, b'e' | b'E') && bytes.get(i + 1) == Some(&b'\'') {
            let start = i;
            i = skip_sql_single_quoted(bytes, i + 1, true);
            line_offset += count_newlines(&bytes[start..i]);
            tokens.push(SqlTok {
                kind: SqlTokKind::Other,
                text: &body[start..i],
                line_offset,
            });
            continue;
        }
        if b == b'$'
            && let Some(tag_bytes) = parse_dollar_tag(bytes, i)
            && let Ok(tag) = std::str::from_utf8(tag_bytes)
        {
            let start = i;
            let inner = i + tag.len();
            i = body[inner..]
                .find(tag)
                .map_or(len, |rel| inner + rel + tag.len());
            line_offset += count_newlines(&bytes[start..i]);
            tokens.push(SqlTok {
                kind: SqlTokKind::Other,
                text: &body[start..i],
                line_offset,
            });
            continue;
        }
        if b == b'"' || b == b'`' {
            let start = i;
            let tok_line = line_offset;
            i = skip_sql_quoted_ident(bytes, i, b);
            line_offset += count_newlines(&bytes[start..i]);
            tokens.push(SqlTok {
                kind: SqlTokKind::Quoted,
                text: &body[start..i],
                line_offset: tok_line,
            });
            continue;
        }
        if let Some((word, end)) = read_sql_word(body, i) {
            tokens.push(SqlTok {
                kind: SqlTokKind::Word,
                text: word,
                line_offset,
            });
            i = end;
            continue;
        }
        let step = body[i..].chars().next().map_or(1, char::len_utf8);
        let kind = if step == 1 {
            match b {
                b'.' => SqlTokKind::Dot,
                b'(' => SqlTokKind::LParen,
                b')' => SqlTokKind::RParen,
                b',' => SqlTokKind::Comma,
                b';' => SqlTokKind::Semi,
                _ => SqlTokKind::Other,
            }
        } else {
            SqlTokKind::Other
        };
        tokens.push(SqlTok {
            kind,
            text: &body[i..i + step],
            line_offset,
        });
        i += step;
    }
    tokens
}

fn upper_ascii_word<'a>(word: &str, buf: &'a mut [u8; 16]) -> Option<&'a str> {
    if word.len() > buf.len() || !word.is_ascii() {
        return None;
    }
    for (dst, src) in buf.iter_mut().zip(word.bytes()) {
        *dst = src.to_ascii_uppercase();
    }
    std::str::from_utf8(&buf[..word.len()]).ok()
}

fn is_non_table_sql_keyword(word: &str) -> bool {
    let mut buf = [0u8; 16];
    let Some(upper) = upper_ascii_word(word, &mut buf) else {
        return false;
    };
    matches!(
        upper,
        "SELECT"
            | "VALUES"
            | "LATERAL"
            | "ONLY"
            | "UNNEST"
            | "ROWS"
            | "TABLE"
            | "WHERE"
            | "SET"
            | "INTO"
            | "ON"
            | "USING"
            | "JOIN"
            | "INNER"
            | "LEFT"
            | "RIGHT"
            | "FULL"
            | "OUTER"
            | "CROSS"
            | "NATURAL"
            | "GROUP"
            | "ORDER"
            | "HAVING"
            | "LIMIT"
            | "OFFSET"
            | "FETCH"
            | "FOR"
            | "UNION"
            | "INTERSECT"
            | "EXCEPT"
            | "RETURNING"
            | "WINDOW"
            | "STDIN"
            | "STDOUT"
            | "PROGRAM"
            | "CURRENT"
            | "DEFAULT"
            | "NULL"
            | "TRUE"
            | "FALSE"
            | "END"
            | "THEN"
            | "ELSE"
            | "LOOP"
            | "BEGIN"
            | "RETURN"
            | "IF"
    )
}

fn is_from_clause_ender(word: &str) -> bool {
    let mut buf = [0u8; 16];
    let Some(upper) = upper_ascii_word(word, &mut buf) else {
        return false;
    };
    matches!(
        upper,
        "WHERE"
            | "JOIN"
            | "INNER"
            | "LEFT"
            | "RIGHT"
            | "FULL"
            | "OUTER"
            | "CROSS"
            | "NATURAL"
            | "GROUP"
            | "ORDER"
            | "HAVING"
            | "LIMIT"
            | "OFFSET"
            | "FETCH"
            | "FOR"
            | "UNION"
            | "INTERSECT"
            | "EXCEPT"
            | "WINDOW"
            | "SET"
            | "INTO"
            | "RETURNING"
            | "ON"
            | "USING"
            | "VALUES"
            | "END"
            | "THEN"
            | "ELSE"
            | "LOOP"
    )
}

fn read_tok_object_ref(tokens: &[SqlTok<'_>], start: usize) -> Option<(String, usize, usize)> {
    let first = tokens.get(start)?;
    if !matches!(first.kind, SqlTokKind::Word | SqlTokKind::Quoted) {
        return None;
    }
    if first.kind == SqlTokKind::Word && is_non_table_sql_keyword(first.text) {
        return None;
    }
    let line_offset = first.line_offset;
    let mut name = sql_name(first.text);
    let mut idx = start + 1;
    while tokens.get(idx).is_some_and(|t| t.kind == SqlTokKind::Dot) {
        let Some(next) = tokens.get(idx + 1) else {
            break;
        };
        if !matches!(next.kind, SqlTokKind::Word | SqlTokKind::Quoted) {
            break;
        }
        name.push('.');
        name.push_str(&sql_name_cow(next.text));
        idx += 2;
    }
    Some((name, line_offset, idx))
}

/// Extracts table references inside a PostgreSQL dollar-quoted function or
/// procedure body, skipping comments, string literals, CTEs, `FOR/DO UPDATE`,
/// `IS DISTINCT FROM`, `EXTRACT(... FROM ...)`, and table-valued functions.
fn extract_sql_body_table_refs(body: &str, body_start_line: usize) -> Vec<(String, usize)> {
    let tokens = tokenize_sql_body(body);
    let mut ctes: HashSet<String> = HashSet::new();
    // Pre-scan CTE names (`WITH [RECURSIVE] name [(cols)] AS (`).
    let mut i = 0usize;
    while i < tokens.len() {
        if tokens[i].kind == SqlTokKind::Word && tokens[i].text.eq_ignore_ascii_case("WITH") {
            let mut j = i + 1;
            if tokens.get(j).is_some_and(|t| {
                t.kind == SqlTokKind::Word && t.text.eq_ignore_ascii_case("RECURSIVE")
            }) {
                j += 1;
            }
            while let Some(cte) = tokens.get(j) {
                if !matches!(cte.kind, SqlTokKind::Word | SqlTokKind::Quoted) {
                    break;
                }
                let cte_name = sql_name(cte.text);
                j += 1;
                // Optional column list `(col1, col2)`
                if tokens.get(j).is_some_and(|t| t.kind == SqlTokKind::LParen) {
                    let mut depth = 1usize;
                    j += 1;
                    while j < tokens.len() && depth > 0 {
                        match tokens[j].kind {
                            SqlTokKind::LParen => depth += 1,
                            SqlTokKind::RParen => depth -= 1,
                            _ => {}
                        }
                        j += 1;
                    }
                }
                if tokens.get(j).is_some_and(|t| {
                    t.kind == SqlTokKind::Word && t.text.eq_ignore_ascii_case("AS")
                }) {
                    j += 1;
                    // Optional `[NOT] MATERIALIZED`
                    if tokens.get(j).is_some_and(|t| {
                        t.kind == SqlTokKind::Word && t.text.eq_ignore_ascii_case("NOT")
                    }) {
                        j += 1;
                    }
                    if tokens.get(j).is_some_and(|t| {
                        t.kind == SqlTokKind::Word && t.text.eq_ignore_ascii_case("MATERIALIZED")
                    }) {
                        j += 1;
                    }
                    if tokens.get(j).is_some_and(|t| t.kind == SqlTokKind::LParen) {
                        ctes.insert(cte_name);
                        let mut depth = 1usize;
                        j += 1;
                        while j < tokens.len() && depth > 0 {
                            match tokens[j].kind {
                                SqlTokKind::LParen => depth += 1,
                                SqlTokKind::RParen => depth -= 1,
                                _ => {}
                            }
                            j += 1;
                        }
                        if tokens.get(j).is_some_and(|t| t.kind == SqlTokKind::Comma) {
                            j += 1;
                            continue;
                        }
                    }
                }
                break;
            }
        }
        i += 1;
    }

    let mut refs = Vec::new();
    let mut non_table_from_depths: Vec<usize> = Vec::new();
    let mut paren_depth = 0usize;
    let mut idx = 0usize;
    while idx < tokens.len() {
        let tok = &tokens[idx];
        match tok.kind {
            SqlTokKind::LParen => {
                paren_depth += 1;
                if idx > 0 && tokens[idx - 1].kind == SqlTokKind::Word {
                    let prev = tokens[idx - 1].text;
                    if prev.eq_ignore_ascii_case("EXTRACT")
                        || prev.eq_ignore_ascii_case("TRIM")
                        || prev.eq_ignore_ascii_case("SUBSTRING")
                        || prev.eq_ignore_ascii_case("OVERLAY")
                    {
                        non_table_from_depths.push(paren_depth);
                    }
                }
                idx += 1;
                continue;
            }
            SqlTokKind::RParen => {
                if non_table_from_depths.last() == Some(&paren_depth) {
                    non_table_from_depths.pop();
                }
                paren_depth = paren_depth.saturating_sub(1);
                idx += 1;
                continue;
            }
            SqlTokKind::Word => {}
            _ => {
                idx += 1;
                continue;
            }
        }

        let word = tok.text;
        let prev_word =
            (idx > 0 && tokens[idx - 1].kind == SqlTokKind::Word).then(|| tokens[idx - 1].text);

        if word.eq_ignore_ascii_case("FROM") {
            let in_non_table_expr = non_table_from_depths.last() == Some(&paren_depth);
            let is_distinct_from = prev_word.is_some_and(|w| w.eq_ignore_ascii_case("DISTINCT"));
            if !in_non_table_expr && !is_distinct_from {
                let mut cur = idx + 1;
                loop {
                    while tokens.get(cur).is_some_and(|t| {
                        t.kind == SqlTokKind::Word
                            && (t.text.eq_ignore_ascii_case("ONLY")
                                || t.text.eq_ignore_ascii_case("LATERAL"))
                    }) {
                        cur += 1;
                    }
                    if let Some((name, line_off, after)) = read_tok_object_ref(&tokens, cur) {
                        let is_call = tokens
                            .get(after)
                            .is_some_and(|t| t.kind == SqlTokKind::LParen);
                        if !is_call && !ctes.contains(&name) {
                            refs.push((name, body_start_line + line_off));
                        }
                        cur = after;
                        // Skip optional `[AS] alias` and check for comma-join `, table2`.
                        if tokens.get(cur).is_some_and(|t| {
                            t.kind == SqlTokKind::Word && t.text.eq_ignore_ascii_case("AS")
                        }) {
                            cur += 1;
                        }
                        if tokens.get(cur).is_some_and(|t| {
                            matches!(t.kind, SqlTokKind::Word | SqlTokKind::Quoted)
                                && (t.kind == SqlTokKind::Quoted || !is_from_clause_ender(t.text))
                        }) {
                            cur += 1;
                        }
                        if tokens.get(cur).is_some_and(|t| t.kind == SqlTokKind::Comma) {
                            cur += 1;
                            continue;
                        }
                    }
                    break;
                }
                idx = cur;
                continue;
            }
        } else if word.eq_ignore_ascii_case("JOIN") {
            let mut cur = idx + 1;
            while tokens.get(cur).is_some_and(|t| {
                t.kind == SqlTokKind::Word
                    && (t.text.eq_ignore_ascii_case("ONLY")
                        || t.text.eq_ignore_ascii_case("LATERAL"))
            }) {
                cur += 1;
            }
            if let Some((name, line_off, after)) = read_tok_object_ref(&tokens, cur) {
                let is_call = tokens
                    .get(after)
                    .is_some_and(|t| t.kind == SqlTokKind::LParen);
                if !is_call && !ctes.contains(&name) {
                    refs.push((name, body_start_line + line_off));
                }
                idx = after;
                continue;
            }
        } else if (word.eq_ignore_ascii_case("INSERT") || word.eq_ignore_ascii_case("MERGE"))
            && tokens
                .get(idx + 1)
                .is_some_and(|t| t.kind == SqlTokKind::Word && t.text.eq_ignore_ascii_case("INTO"))
        {
            let mut cur = idx + 2;
            if tokens
                .get(cur)
                .is_some_and(|t| t.kind == SqlTokKind::Word && t.text.eq_ignore_ascii_case("ONLY"))
            {
                cur += 1;
            }
            if let Some((name, line_off, after)) = read_tok_object_ref(&tokens, cur) {
                if !ctes.contains(&name) {
                    refs.push((name, body_start_line + line_off));
                }
                idx = after;
                continue;
            }
        } else if word.eq_ignore_ascii_case("UPDATE") {
            let skip = prev_word.is_some_and(|w| {
                w.eq_ignore_ascii_case("FOR")
                    || w.eq_ignore_ascii_case("DO")
                    || w.eq_ignore_ascii_case("ON")
                    || w.eq_ignore_ascii_case("BEFORE")
                    || w.eq_ignore_ascii_case("AFTER")
                    || w.eq_ignore_ascii_case("OF")
                    || w.eq_ignore_ascii_case("OR")
            });
            if !skip {
                let mut cur = idx + 1;
                if tokens.get(cur).is_some_and(|t| {
                    t.kind == SqlTokKind::Word && t.text.eq_ignore_ascii_case("ONLY")
                }) {
                    cur += 1;
                }
                if let Some((name, line_off, after)) = read_tok_object_ref(&tokens, cur) {
                    if !ctes.contains(&name) {
                        refs.push((name, body_start_line + line_off));
                    }
                    idx = after;
                    continue;
                }
            }
        } else if word.eq_ignore_ascii_case("TRUNCATE") {
            let mut cur = idx + 1;
            if tokens
                .get(cur)
                .is_some_and(|t| t.kind == SqlTokKind::Word && t.text.eq_ignore_ascii_case("TABLE"))
            {
                cur += 1;
            }
            if tokens
                .get(cur)
                .is_some_and(|t| t.kind == SqlTokKind::Word && t.text.eq_ignore_ascii_case("ONLY"))
            {
                cur += 1;
            }
            if let Some((name, line_off, after)) = read_tok_object_ref(&tokens, cur) {
                if !ctes.contains(&name) {
                    refs.push((name, body_start_line + line_off));
                }
                idx = after;
                continue;
            }
        } else if (word.eq_ignore_ascii_case("ALTER") || word.eq_ignore_ascii_case("DROP"))
            && tokens
                .get(idx + 1)
                .is_some_and(|t| t.kind == SqlTokKind::Word && t.text.eq_ignore_ascii_case("TABLE"))
        {
            let mut cur = idx + 2;
            if tokens
                .get(cur)
                .is_some_and(|t| t.kind == SqlTokKind::Word && t.text.eq_ignore_ascii_case("IF"))
                && tokens.get(cur + 1).is_some_and(|t| {
                    t.kind == SqlTokKind::Word && t.text.eq_ignore_ascii_case("EXISTS")
                })
            {
                cur += 2;
            }
            if tokens
                .get(cur)
                .is_some_and(|t| t.kind == SqlTokKind::Word && t.text.eq_ignore_ascii_case("ONLY"))
            {
                cur += 1;
            }
            if let Some((name, line_off, after)) = read_tok_object_ref(&tokens, cur) {
                if !ctes.contains(&name) {
                    refs.push((name, body_start_line + line_off));
                }
                idx = after;
                continue;
            }
        } else if (word.eq_ignore_ascii_case("REFERENCES") || word.eq_ignore_ascii_case("COPY"))
            && let Some((name, line_off, after)) = read_tok_object_ref(&tokens, idx + 1)
        {
            if !ctes.contains(&name) {
                refs.push((name, body_start_line + line_off));
            }
            idx = after;
            continue;
        }

        idx += 1;
    }
    refs
}

// ---------------------------------------------------------------------------
// Helm chart `values.yaml` & Go-template extraction (zero regex, UTF-8 safe)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HelmRole {
    Values,
    Template,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HelmFacts {
    pub chart_root: String,
    pub lines: usize,
    pub definitions: Vec<Definition>,
    pub references: Vec<Reference>,
}

fn normalized_rel_path(file: &str) -> Cow<'_, str> {
    if file.contains('\\') {
        Cow::Owned(file.replace('\\', "/"))
    } else {
        Cow::Borrowed(file)
    }
}

/// Identifies every Helm chart root directory in `files` by locating `Chart.yaml`
/// (or `Chart.yml`) manifests outside `templates/` directories.
pub fn helm_chart_roots(files: &BTreeSet<String>) -> BTreeSet<String> {
    let mut roots = BTreeSet::new();
    for file in files {
        let norm = normalized_rel_path(file);
        let (dir, name) = norm.rsplit_once('/').unwrap_or(("", norm.as_ref()));
        if (name == "Chart.yaml" || name == "Chart.yml")
            && dir != "templates"
            && !dir.ends_with("/templates")
        {
            roots.insert(dir.to_owned());
        }
    }
    roots
}

fn is_helm_values_file(name: &str) -> bool {
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return false;
    };
    if !ext.eq_ignore_ascii_case("yaml") && !ext.eq_ignore_ascii_case("yml") {
        return false;
    }
    stem.eq_ignore_ascii_case("values")
        || stem.get(..7).is_some_and(|prefix| {
            prefix.eq_ignore_ascii_case("values.") || prefix.eq_ignore_ascii_case("values-")
        })
}

fn is_helm_template_file(rel: &str) -> bool {
    let Some(under_templates) = rel.strip_prefix("templates/") else {
        return false;
    };
    let name = under_templates
        .rsplit_once('/')
        .map_or(under_templates, |(_, file)| file);
    let Some((_, ext)) = name.rsplit_once('.') else {
        return false;
    };
    ext.eq_ignore_ascii_case("yaml")
        || ext.eq_ignore_ascii_case("yml")
        || ext.eq_ignore_ascii_case("tpl")
}

/// Classifies `file` when it belongs to one of `chart_roots`: either a chart
/// `values*.yaml` file alongside `Chart.yaml`, or a template under `templates/`.
/// Plain non-Helm YAML files return `None`.
pub fn helm_file_role(file: &str, chart_roots: &BTreeSet<String>) -> Option<(String, HelmRole)> {
    if chart_roots.is_empty() {
        return None;
    }
    let norm = normalized_rel_path(file);
    let mut best: Option<(&str, &str)> = None;
    for root in chart_roots {
        let rel = if root.is_empty() {
            Some(norm.as_ref())
        } else {
            norm.strip_prefix(root.as_str())
                .and_then(|rest| rest.strip_prefix('/'))
        };
        if let Some(rel) = rel
            && best.is_none_or(|(prev_root, _)| root.len() >= prev_root.len())
        {
            best = Some((root.as_str(), rel));
        }
    }
    let (chart_root, rel) = best?;
    if !rel.contains('/') && is_helm_values_file(rel) {
        Some((chart_root.to_owned(), HelmRole::Values))
    } else if is_helm_template_file(rel) {
        Some((chart_root.to_owned(), HelmRole::Template))
    } else {
        None
    }
}

pub fn parse_helm(chart_root: String, role: HelmRole, source: &str) -> HelmFacts {
    match role {
        HelmRole::Values => parse_helm_values(chart_root, source),
        HelmRole::Template => parse_helm_template(chart_root, source),
    }
}

fn parse_yaml_mapping_key(rest: &str) -> Option<(&str, &str)> {
    let first = rest.chars().next()?;
    if first == '"' {
        let mut escaped = false;
        for (idx, ch) in rest.char_indices().skip(1) {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                let key = &rest[1..idx];
                let after_quote = rest[idx + 1..].trim_start_matches([' ', '\t']);
                let after_colon = after_quote.strip_prefix(':')?;
                if after_colon.is_empty() || after_colon.starts_with([' ', '\t']) {
                    return (!key.is_empty()).then_some((key, after_colon));
                }
                return None;
            }
        }
        return None;
    }
    if first == '\'' {
        let mut iter = rest.char_indices().skip(1).peekable();
        while let Some((idx, ch)) = iter.next() {
            if ch == '\'' {
                if iter.peek().is_some_and(|&(_, next)| next == '\'') {
                    iter.next();
                    continue;
                }
                let key = &rest[1..idx];
                let after_quote = rest[idx + 1..].trim_start_matches([' ', '\t']);
                let after_colon = after_quote.strip_prefix(':')?;
                if after_colon.is_empty() || after_colon.starts_with([' ', '\t']) {
                    return (!key.is_empty()).then_some((key, after_colon));
                }
                return None;
            }
        }
        return None;
    }
    if matches!(
        first,
        '#' | '-' | '{' | '}' | '[' | ']' | '&' | '*' | '!' | '?' | '%' | '@' | '`'
    ) || rest.starts_with("{{")
    {
        return None;
    }
    for (idx, ch) in rest.char_indices() {
        if ch == ':' {
            let after_colon = &rest[idx + 1..];
            if after_colon.is_empty() || after_colon.starts_with([' ', '\t']) {
                let key = rest[..idx].trim_end_matches([' ', '\t']);
                if key.is_empty()
                    || key.contains(['{', '}', '[', ']', ',', '#'])
                    || key.contains("{{")
                {
                    return None;
                }
                return Some((key, after_colon));
            }
        }
    }
    None
}

fn is_yaml_block_scalar(after_colon: &str) -> bool {
    let val = after_colon.trim_start_matches([' ', '\t']);
    let Some(rest) = val.strip_prefix('|').or_else(|| val.strip_prefix('>')) else {
        return false;
    };
    let header = rest.split([' ', '\t', '#']).next().unwrap_or("");
    header.chars().all(|ch| matches!(ch, '-' | '+' | '1'..='9'))
}

fn parse_helm_values(chart_root: String, source: &str) -> HelmFacts {
    let lines = source.lines().count().max(1);
    let mut definitions: Vec<Definition> = Vec::new();
    let mut active_l0: Option<(usize, String)> = None;
    let mut active_l1: Option<(usize, usize)> = None;
    let mut section_child_indent: Option<usize> = None;
    let mut block_scalar_indent: Option<usize> = None;
    let mut sequence_indent: Option<usize> = None;

    let mut byte_offset = 0usize;
    for (idx, raw_line) in source.split_inclusive('\n').enumerate() {
        let line_no = idx + 1;
        let line_start = byte_offset;
        byte_offset += raw_line.len();
        let line = raw_line.trim_end_matches(['\r', '\n']);
        let line_end = line_start + line.len();
        let rest = line.trim_start_matches([' ', '\t']);
        if rest.is_empty() {
            continue;
        }
        let indent = line.len() - rest.len();

        if let Some(parent_indent) = block_scalar_indent {
            if indent > parent_indent {
                if let Some((l0_idx, _)) = active_l0 {
                    definitions[l0_idx].end = line_no;
                    definitions[l0_idx].bytes.end = line_end;
                }
                if let Some((l1_idx, l1_indent)) = active_l1
                    && parent_indent >= l1_indent
                {
                    definitions[l1_idx].end = line_no;
                    definitions[l1_idx].bytes.end = line_end;
                }
                continue;
            }
            block_scalar_indent = None;
        }

        if rest.starts_with('#') {
            continue;
        }
        if rest == "---" || rest == "..." || rest.starts_with("--- ") || rest.starts_with("... ") {
            active_l0 = None;
            active_l1 = None;
            section_child_indent = None;
            sequence_indent = None;
            continue;
        }

        let is_seq_item = rest == "-" || rest.starts_with("- ") || rest.starts_with("-\t");
        if let Some(seq_indent) = sequence_indent {
            if indent > seq_indent || (indent == seq_indent && is_seq_item) {
                if indent > 0 {
                    if let Some((l0_idx, _)) = active_l0 {
                        definitions[l0_idx].end = line_no;
                        definitions[l0_idx].bytes.end = line_end;
                    }
                    if let Some((l1_idx, l1_indent)) = active_l1
                        && seq_indent > l1_indent
                    {
                        definitions[l1_idx].end = line_no;
                        definitions[l1_idx].bytes.end = line_end;
                    }
                }
                continue;
            }
            sequence_indent = None;
        }
        if is_seq_item {
            if indent > 0 {
                if let Some((l0_idx, _)) = active_l0 {
                    definitions[l0_idx].end = line_no;
                    definitions[l0_idx].bytes.end = line_end;
                }
                if let Some((l1_idx, l1_indent)) = active_l1
                    && indent > l1_indent
                {
                    definitions[l1_idx].end = line_no;
                    definitions[l1_idx].bytes.end = line_end;
                }
                sequence_indent = Some(indent);
            } else {
                active_l0 = None;
                active_l1 = None;
                section_child_indent = None;
            }
            continue;
        }

        if indent == 0 {
            active_l0 = None;
            active_l1 = None;
            section_child_indent = None;
            if let Some((key, after_colon)) = parse_yaml_mapping_key(rest) {
                let full = format!("values.{key}");
                let def_idx = definitions.len();
                definitions.push(Definition {
                    bytes: line_start..line_end,
                    signature_start: line_start,
                    header_end: line_end,
                    name: full.clone(),
                    path: full,
                    kind: "value".to_owned(),
                    start: line_no,
                    end: line_no,
                    declaration: line_no,
                    test: false,
                });
                active_l0 = Some((def_idx, key.to_owned()));
                if is_yaml_block_scalar(after_colon) {
                    block_scalar_indent = Some(0);
                }
            }
            continue;
        }

        if let Some((l0_idx, ref l0_key)) = active_l0 {
            definitions[l0_idx].end = line_no;
            definitions[l0_idx].bytes.end = line_end;
            match section_child_indent {
                None => {
                    if let Some((key, after_colon)) = parse_yaml_mapping_key(rest) {
                        section_child_indent = Some(indent);
                        let full = format!("values.{l0_key}.{key}");
                        let def_idx = definitions.len();
                        definitions.push(Definition {
                            bytes: line_start..line_end,
                            signature_start: line_start,
                            header_end: line_end,
                            name: full.clone(),
                            path: full,
                            kind: "value".to_owned(),
                            start: line_no,
                            end: line_no,
                            declaration: line_no,
                            test: false,
                        });
                        active_l1 = Some((def_idx, indent));
                        if is_yaml_block_scalar(after_colon) {
                            block_scalar_indent = Some(indent);
                        }
                    }
                }
                Some(child_indent) if indent == child_indent => {
                    if let Some((key, after_colon)) = parse_yaml_mapping_key(rest) {
                        let full = format!("values.{l0_key}.{key}");
                        let def_idx = definitions.len();
                        definitions.push(Definition {
                            bytes: line_start..line_end,
                            signature_start: line_start,
                            header_end: line_end,
                            name: full.clone(),
                            path: full,
                            kind: "value".to_owned(),
                            start: line_no,
                            end: line_no,
                            declaration: line_no,
                            test: false,
                        });
                        active_l1 = Some((def_idx, indent));
                        if is_yaml_block_scalar(after_colon) {
                            block_scalar_indent = Some(indent);
                        }
                    } else if let Some((l1_idx, _)) = active_l1 {
                        definitions[l1_idx].end = line_no;
                        definitions[l1_idx].bytes.end = line_end;
                    }
                }
                Some(child_indent) if indent > child_indent => {
                    if let Some((l1_idx, _)) = active_l1 {
                        definitions[l1_idx].end = line_no;
                        definitions[l1_idx].bytes.end = line_end;
                    }
                    if let Some((_, after_colon)) = parse_yaml_mapping_key(rest)
                        && is_yaml_block_scalar(after_colon)
                    {
                        block_scalar_indent = Some(indent);
                    }
                }
                Some(_) => {}
            }
        }
    }

    HelmFacts {
        chart_root,
        lines,
        definitions,
        references: Vec::new(),
    }
}

enum HelmToken<'a> {
    Word(&'a str, usize),
    Str(String, usize),
    Punct,
}

fn tokenize_helm_action(body: &str, start_line: usize) -> Vec<HelmToken<'_>> {
    let mut tokens = Vec::new();
    let mut line = start_line;
    let mut chars = body.char_indices().peekable();
    while let Some(&(idx, ch)) = chars.peek() {
        if ch.is_whitespace() {
            if ch == '\n' {
                line += 1;
            }
            chars.next();
            continue;
        }
        if ch == '/' && body[idx..].starts_with("/*") {
            chars.next();
            chars.next();
            while let Some((c_idx, c)) = chars.next() {
                if c == '\n' {
                    line += 1;
                }
                if c == '*' && body[c_idx + 1..].starts_with('/') {
                    chars.next();
                    break;
                }
            }
            continue;
        }
        if ch == '"' {
            let tok_line = line;
            chars.next();
            let mut decoded = String::new();
            let mut escaped = false;
            for (_, c) in chars.by_ref() {
                if c == '\n' {
                    line += 1;
                }
                if escaped {
                    match c {
                        'n' => decoded.push('\n'),
                        'r' => decoded.push('\r'),
                        't' => decoded.push('\t'),
                        other => decoded.push(other),
                    }
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    break;
                } else {
                    decoded.push(c);
                }
            }
            tokens.push(HelmToken::Str(decoded, tok_line));
            continue;
        }
        if ch == '`' {
            let tok_line = line;
            chars.next();
            let content_start = idx + 1;
            let mut content_end = body.len();
            for (c_idx, c) in chars.by_ref() {
                if c == '\n' {
                    line += 1;
                }
                if c == '`' {
                    content_end = c_idx;
                    break;
                }
            }
            tokens.push(HelmToken::Str(
                body[content_start..content_end].to_owned(),
                tok_line,
            ));
            continue;
        }
        if matches!(ch, '(' | ')' | '|' | ',' | ';') {
            chars.next();
            tokens.push(HelmToken::Punct);
            continue;
        }
        let tok_line = line;
        let word_start = idx;
        let mut word_end = body.len();
        while let Some(&(c_idx, c)) = chars.peek() {
            if c.is_whitespace()
                || matches!(c, '(' | ')' | '|' | ',' | ';' | '"' | '`')
                || (c == '/' && body[c_idx..].starts_with("/*"))
            {
                word_end = c_idx;
                break;
            }
            chars.next();
        }
        if word_end > word_start {
            tokens.push(HelmToken::Word(&body[word_start..word_end], tok_line));
        }
    }
    tokens
}

fn helm_values_ref_target(word: &str) -> Option<String> {
    let rest = word
        .strip_prefix(".Values.")
        .or_else(|| word.strip_prefix("$.Values."))?;
    let mut end = rest.len();
    for (idx, ch) in rest.char_indices() {
        if !(ch == '.' || ch == '_' || ch.is_alphanumeric()) {
            end = idx;
            break;
        }
    }
    let clean = rest[..end].trim_end_matches('.');
    if clean.is_empty() || clean.split('.').any(str::is_empty) {
        return None;
    }
    Some(format!("values.{clean}"))
}

fn parse_helm_template(chart_root: String, source: &str) -> HelmFacts {
    let lines = source.lines().count().max(1);
    let mut definitions: Vec<Definition> = Vec::new();
    let mut references: Vec<Reference> = Vec::new();
    let mut seen: HashSet<(u8, usize, Option<usize>, String)> = HashSet::new();
    let mut define_stack: Vec<(usize, usize)> = Vec::new();

    let bytes = source.as_bytes();
    let mut i = 0usize;
    let mut line = 1usize;

    while i < bytes.len() {
        if bytes[i] == b'\n' {
            line += 1;
            i += 1;
            continue;
        }
        if bytes[i] == b'{' && bytes.get(i + 1) == Some(&b'{') {
            let action_start_byte = i;
            let action_start_line = line;
            let inner_start = i + 2;
            let mut j = inner_start;
            let mut end_line = line;
            let mut in_quote = false;
            let mut in_raw = false;
            let mut in_comment = false;
            let mut closed_at = None;

            while j < bytes.len() {
                let b = bytes[j];
                if b == b'\n' {
                    end_line += 1;
                }
                if in_comment {
                    if b == b'*' && bytes.get(j + 1) == Some(&b'/') {
                        in_comment = false;
                        j += 2;
                        continue;
                    }
                    j += 1;
                    continue;
                }
                if in_raw {
                    if b == b'`' {
                        in_raw = false;
                    }
                    j += 1;
                    continue;
                }
                if in_quote {
                    if b == b'\\' {
                        j += 1;
                        if j < bytes.len() {
                            if bytes[j] == b'\n' {
                                end_line += 1;
                            }
                            j += source[j..].chars().next().map_or(1, char::len_utf8);
                        }
                        continue;
                    }
                    if b == b'"' {
                        in_quote = false;
                    }
                    j += 1;
                    continue;
                }
                if b == b'/' && bytes.get(j + 1) == Some(&b'*') {
                    in_comment = true;
                    j += 2;
                    continue;
                }
                if b == b'"' {
                    in_quote = true;
                    j += 1;
                    continue;
                }
                if b == b'`' {
                    in_raw = true;
                    j += 1;
                    continue;
                }
                if b == b'}' && bytes.get(j + 1) == Some(&b'}') {
                    closed_at = Some(j);
                    break;
                }
                j += 1;
            }

            let Some(inner_end) = closed_at else {
                break;
            };
            let action_end_byte = inner_end + 2;
            let action_end_line = end_line;

            let mut body = &source[inner_start..inner_end];
            if body.starts_with('-')
                && body[1..].starts_with(|ch: char| ch.is_whitespace() || ch == '/')
            {
                body = &body[1..];
            }
            if body.ends_with('-')
                && (body.len() == 1
                    || body[..body.len() - 1].ends_with(|ch: char| ch.is_whitespace() || ch == '*'))
            {
                body = &body[..body.len() - 1];
            }

            let tokens = tokenize_helm_action(body, action_start_line);
            if !tokens.is_empty() {
                match tokens.first() {
                    Some(HelmToken::Word("define", def_line)) => {
                        if let Some(HelmToken::Str(name, _)) = tokens.get(1)
                            && !name.is_empty()
                        {
                            let def_idx = definitions.len();
                            definitions.push(Definition {
                                bytes: action_start_byte..action_end_byte,
                                signature_start: action_start_byte,
                                header_end: action_end_byte,
                                name: name.clone(),
                                path: name.clone(),
                                kind: "template".to_owned(),
                                start: action_start_line,
                                end: action_end_line,
                                declaration: *def_line,
                                test: false,
                            });
                            define_stack.push((def_idx, 1));
                        }
                    }
                    Some(HelmToken::Word("if" | "range" | "with" | "block", _)) => {
                        if let Some((_, depth)) = define_stack.last_mut() {
                            *depth += 1;
                        }
                    }
                    Some(HelmToken::Word("end", _)) => {
                        if let Some((def_idx, depth)) = define_stack.last_mut() {
                            *depth -= 1;
                            if *depth == 0 {
                                let idx = *def_idx;
                                definitions[idx].end = action_end_line;
                                definitions[idx].bytes.end = action_end_byte;
                                define_stack.pop();
                            }
                        }
                    }
                    _ => {}
                }

                let from = define_stack.last().map(|&(def_idx, _)| def_idx);
                let mut push_ref = |name: String, ref_line: usize, kind: Kind| {
                    if seen.insert((kind as u8, ref_line, from, name.clone())) {
                        references.push(Reference {
                            name,
                            from,
                            line: ref_line,
                            kind,
                        });
                    }
                };

                for (tok_idx, tok) in tokens.iter().enumerate() {
                    let HelmToken::Word(word, tok_line) = tok else {
                        continue;
                    };
                    if (*word == "include" || *word == "template")
                        && let Some(HelmToken::Str(target, str_line)) = tokens.get(tok_idx + 1)
                        && !target.is_empty()
                    {
                        push_ref(target.clone(), *str_line, Kind::Call);
                        continue;
                    }
                    if *word == "index"
                        && let Some(HelmToken::Word(".Values" | "$.Values", _)) =
                            tokens.get(tok_idx + 1)
                        && let Some(HelmToken::Str(k1, str_line)) = tokens.get(tok_idx + 2)
                        && !k1.is_empty()
                    {
                        let target = match tokens.get(tok_idx + 3) {
                            Some(HelmToken::Str(k2, _)) if !k2.is_empty() => {
                                format!("values.{k1}.{k2}")
                            }
                            _ => format!("values.{k1}"),
                        };
                        push_ref(target, *str_line, Kind::Reference);
                        continue;
                    }
                    if let Some(target) = helm_values_ref_target(word) {
                        push_ref(target, *tok_line, Kind::Reference);
                    }
                }
            }

            line = action_end_line;
            i = action_end_byte;
            continue;
        }
        i += 1;
    }

    for (def_idx, _) in define_stack {
        definitions[def_idx].end = lines;
        definitions[def_idx].bytes.end = source.len();
    }

    HelmFacts {
        chart_root,
        lines,
        definitions,
        references,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grammars_come_from_names_shebangs_and_objective_c_markers() {
        for (file, text, grammar) in [
            (
                "bin/deploy",
                "#!/usr/bin/env python3.12\n",
                Some(Grammar::Python),
            ),
            ("bin/run", "#!/bin/zsh -e\n", Some(Grammar::Bash)),
            (
                "bin/serve",
                "#!/usr/bin/env -S node --no-warnings\n",
                Some(Grammar::JavaScript),
            ),
            (
                "bin/task",
                "#!/usr/bin/env -S deno run\n",
                Some(Grammar::TypeScript),
            ),
            ("bin/report", "#!/usr/bin/ruby\n", Some(Grammar::Ruby)),
            (
                "bin/setup",
                "#!/usr/bin/env pwsh\n",
                Some(Grammar::PowerShell),
            ),
            ("bin/legacy", "#!/usr/bin/perl\n", None),
            ("bin/notes", "plain text\n", None),
            ("Jenkinsfile", "pipeline {}\n", Some(Grammar::Groovy)),
            // An extension wins over a shebang.
            ("tool.py", "#!/bin/sh\n", Some(Grammar::Python)),
            (
                "Thing.h",
                "#import <Foundation/Foundation.h>\n",
                Some(Grammar::ObjectiveC),
            ),
            ("thing.h", "int thing(void);\n", Some(Grammar::C)),
            (
                "Thing.m",
                "@implementation Thing\n@end\n",
                Some(Grammar::ObjectiveC),
            ),
            ("total.m", "function y = total(x)\n  y = x;\nend\n", None),
            ("build.gradle", "", Some(Grammar::Groovy)),
            ("App.vue", "", Some(Grammar::Vue)),
        ] {
            assert_eq!(Grammar::detect(file, text), grammar, "{file}");
        }
    }

    #[test]
    fn bare_macro_lines_are_blanked_in_place() {
        let source =
            "NS_ASSUME_NONNULL_BEGIN\r\nNS_HEADER_AUDIT_BEGIN(nullability)\nFOO_BAR(x);\n@end\n";
        let blanked = blank_bare_macros(source);
        assert_eq!(blanked.len(), source.len());
        assert_eq!(
            blanked.split_whitespace().collect::<Vec<_>>(),
            ["FOO_BAR(x);", "@end"]
        );
    }

    #[test]
    fn outline_headers_trim_go_struct_bodies_and_long_polyglot_initializers() {
        let mut parsers = Parsers::default();

        let go_src = concat!(
            "package server\n\n",
            "type Server struct {\n",
            "    addr string\n",
            "    port int\n",
            "}\n\n",
            "type Handler interface {\n",
            "    Serve(req string) error\n",
            "}\n",
        );
        let go_facts = parsers.parse(Grammar::Go, go_src).unwrap();
        let sig = |facts: &Facts, src: &str, name: &str| {
            let def = facts.definitions.iter().find(|d| d.name == name).unwrap();
            src[def.signature_start..def.header_end]
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert_eq!(sig(&go_facts, go_src, "Server"), "Server struct");
        assert_eq!(sig(&go_facts, go_src, "Handler"), "Handler interface");

        let ts_src = concat!(
            "export type UserId = string;\n",
            "export type ComplexConfig = {\n",
            "  endpoint: string;\n",
            "  retries: number;\n",
            "  timeoutMs: number;\n",
            "};\n",
            "export const handle = (id: UserId): Promise<void> => {\n",
            "  return Promise.resolve();\n",
            "};\n",
        );
        let ts_facts = parsers.parse(Grammar::TypeScript, ts_src).unwrap();
        assert_eq!(
            sig(&ts_facts, ts_src, "UserId"),
            "export type UserId = string;"
        );
        assert_eq!(
            sig(&ts_facts, ts_src, "ComplexConfig"),
            "export type ComplexConfig"
        );
        assert_eq!(
            sig(&ts_facts, ts_src, "handle"),
            "export const handle = (id: UserId): Promise<void> =>"
        );

        let kt_src = concat!(
            "val LIMIT = 42\n",
            "val DEFAULT_RULES: List<String> = listOf(\n",
            "    \"rule-one\",\n",
            "    \"rule-two\",\n",
            ")\n",
        );
        let kt_facts = parsers.parse(Grammar::Kotlin, kt_src).unwrap();
        assert_eq!(sig(&kt_facts, kt_src, "LIMIT"), "val LIMIT = 42");
        assert_eq!(
            sig(&kt_facts, kt_src, "DEFAULT_RULES"),
            "val DEFAULT_RULES: List<String>"
        );
    }

    #[test]
    fn js_ts_tsx_test_blocks_extract_suites_tests_modifiers_and_each() {
        let mut parsers = Parsers::default();
        let ts_src = concat!(
            "import { describe, it, test, expect, beforeEach } from 'vitest';\n\n",
            "describe('OrderService', () => {\n",
            "  beforeEach(() => {});\n",
            "  it('creates an order', async () => {\n",
            "    expect(1).toBe(1);\n",
            "  });\n",
            "  it.skip('handles timeout', () => {\n",
            "    expect(true).toBe(true);\n",
            "  });\n",
            "  test.only(`retries ${3} times`, () => {\n",
            "    expect(3).toBe(3);\n",
            "  });\n",
            "  test.each([1, 2, 3])('handles %i', (n) => {\n",
            "    expect(n).toBeGreaterThan(0);\n",
            "  });\n",
            "  it.each`\n",
            "    a | b\n",
            "    ${1} | ${2}\n",
            "  `('adds $a and $b', ({ a, b }) => {\n",
            "    expect(a + b).toBeGreaterThan(0);\n",
            "  });\n",
            "});\n\n",
            "test.describe.only('Playwright suite', () => {\n",
            "  test('renders page', async () => {\n",
            "    expect(1).toBe(1);\n",
            "  });\n",
            "});\n",
        );
        for grammar in [Grammar::JavaScript, Grammar::TypeScript, Grammar::Tsx] {
            let facts = parsers.parse(grammar, ts_src).unwrap();
            let tests: Vec<_> = facts
                .definitions
                .iter()
                .filter(|d| d.kind == "test")
                .map(|d| (d.path.as_str(), d.test))
                .collect();
            assert_eq!(
                tests,
                [
                    ("\"OrderService\"", true),
                    ("\"OrderService\"::\"creates an order\"", true),
                    ("\"OrderService\"::\"handles timeout\"", true),
                    ("\"OrderService\"::\"retries ${3} times\"", true),
                    ("\"OrderService\"::\"handles %i\"", true),
                    ("\"OrderService\"::\"adds $a and $b\"", true),
                    ("\"Playwright suite\"", true),
                    ("\"Playwright suite\"::\"renders page\"", true),
                ],
                "{grammar:?}"
            );
            for def in facts.definitions.iter().filter(|d| d.kind == "test") {
                let slice = &ts_src[def.bytes.clone()];
                assert!(
                    slice.ends_with(')'),
                    "expected exact call_expression bytes for {}: {slice:?}",
                    def.path
                );
            }
        }
    }

    #[test]
    fn large_vitest_file_has_zero_uncovered_code() {
        let mut src = String::from(
            "import { describe, it, test, expect, beforeEach } from 'vitest';\nimport { calc } from './calc';\n\n",
        );
        for suite in 0..12 {
            src.push_str(&format!("describe('suite {suite}', () => {{\n"));
            src.push_str("  beforeEach(() => {\n    calc(0);\n  });\n");
            for case in 0..8 {
                src.push_str(&format!(
                    "  it('case {suite}-{case}', () => {{\n    const a = calc({case});\n    const b = a + 1;\n    const c = b * 2;\n    const d = c - 1;\n    expect(d).toBeGreaterThan(0);\n  }});\n"
                ));
            }
            src.push_str(&format!(
                "  test.each([1, 2, 3])('table {suite} %i', (n) => {{\n    expect(calc(n)).toBeDefined();\n  }});\n"
            ));
            src.push_str("});\n\n");
        }
        assert!(
            src.lines().count() >= 700,
            "expected >= 700 lines, got {}",
            src.lines().count()
        );
        let mut snap = crate::source::Snapshot::from_text("src/calc.test.ts", src.clone());
        snap.index("src/calc.test.ts").unwrap();
        let uncovered = crate::agents::uncovered_code("src/calc.test.ts", &snap).unwrap();
        assert!(
            uncovered.is_empty(),
            "expected zero uncovered lines in 700-line Vitest file, got: {uncovered:?}"
        );
    }

    #[test]
    fn ruby_rspec_blocks_extract_describe_context_it_specify() {
        let mut parsers = Parsers::default();
        let src = concat!(
            "require 'spec_helper'\n\n",
            "RSpec.describe MyApp::OrderService do\n",
            "  describe '#total' do\n",
            "    context 'when discounted' do\n",
            "      it 'applies discount' do\n",
            "        expect(1).to eq(1)\n",
            "      end\n\n",
            "      specify('handles zero') { expect(0).to eq(0) }\n",
            "    end\n",
            "  end\n",
            "end\n\n",
            "other.describe('not rspec') do\n",
            "  puts 'ignored'\n",
            "end\n",
        );
        let facts = parsers.parse(Grammar::Ruby, src).unwrap();
        let tests: Vec<_> = facts
            .definitions
            .iter()
            .filter(|d| d.kind == "test")
            .map(|d| (d.path.as_str(), d.test))
            .collect();
        assert_eq!(
            tests,
            [
                ("\"MyApp::OrderService\"", true),
                ("\"MyApp::OrderService\"::\"#total\"", true),
                (
                    "\"MyApp::OrderService\"::\"#total\"::\"when discounted\"",
                    true
                ),
                (
                    "\"MyApp::OrderService\"::\"#total\"::\"when discounted\"::\"applies discount\"",
                    true
                ),
                (
                    "\"MyApp::OrderService\"::\"#total\"::\"when discounted\"::\"handles zero\"",
                    true
                ),
            ]
        );
        let rspec_only = src.split("\nother.describe").next().unwrap().to_owned() + "\n";
        let mut snap = crate::source::Snapshot::from_text("spec/order_spec.rb", rspec_only);
        snap.index("spec/order_spec.rb").unwrap();
        let uncovered = crate::agents::uncovered_code("spec/order_spec.rb", &snap).unwrap();
        assert!(uncovered.is_empty(), "uncovered: {uncovered:?}");
    }

    #[test]
    fn sql_extended_ddl_extracts_functions_procedures_triggers_types_views_and_schemas() {
        let mut parsers = Parsers::default();
        let sql = concat!(
            "CREATE SCHEMA IF NOT EXISTS app;\n",
            "CREATE SCHEMA billing AUTHORIZATION admin_role;\n",
            "CREATE SCHEMA AUTHORIZATION solo_role;\n\n",
            "CREATE TYPE app.order_status AS ENUM ('pending', 'paid');\n\n",
            "CREATE TABLE app.orders (\n",
            "  id INT PRIMARY KEY,\n",
            "  status app.order_status\n",
            ");\n\n",
            "CREATE VIEW app.open_orders AS\n",
            "SELECT id FROM app.orders;\n\n",
            "CREATE MATERIALIZED VIEW app.daily_totals AS\n",
            "SELECT COUNT(*) AS cnt FROM app.orders;\n\n",
            "CREATE OR REPLACE FUNCTION app.order_status_of(p_id INT)\n",
            "RETURNS app.order_status\n",
            "LANGUAGE plpgsql AS $$\n",
            "BEGIN\n",
            "  RETURN 'paid';\n",
            "END;\n",
            "$$;\n\n",
            "CREATE PROCEDURE app.close_order(p_id INT)\n",
            "LANGUAGE plpgsql AS $$\n",
            "BEGIN\n",
            "  UPDATE app.orders SET status = 'paid' WHERE id = p_id;\n",
            "END;\n",
            "$$;\n\n",
            "CREATE TRIGGER trg_orders_audit\n",
            "AFTER INSERT ON app.orders\n",
            "FOR EACH ROW\n",
            "EXECUTE FUNCTION app.order_status_of();\n",
        );
        let facts = parsers.parse(Grammar::Sql, sql).unwrap();
        let defs: Vec<_> = facts
            .definitions
            .iter()
            .map(|d| (d.kind.as_str(), d.name.as_str()))
            .collect();
        assert_eq!(
            defs,
            [
                ("schema", "app"),
                ("schema", "billing"),
                ("schema", "solo_role"),
                ("type", "app.order_status"),
                ("table", "app.orders"),
                ("view", "app.open_orders"),
                ("view", "app.daily_totals"),
                ("function", "app.order_status_of"),
                ("procedure", "app.close_order"),
                ("trigger", "trg_orders_audit"),
            ]
        );
        assert!(
            facts
                .references
                .iter()
                .any(|r| r.kind == Kind::Data && r.name == "app.orders")
        );
        for def in &facts.definitions {
            let slice = &sql[def.bytes.clone()];
            assert!(
                slice.starts_with("CREATE"),
                "expected exact CREATE statement bytes for {}: {slice:?}",
                def.name
            );
        }
    }

    #[test]
    fn compiled_queries_are_cached_across_parsers_and_component_grammars() {
        let ts = Grammar::TypeScript.compiled().unwrap();
        let ts_again = Grammar::TypeScript.compiled().unwrap();
        assert!(std::ptr::eq(ts, ts_again));
        for component in [Grammar::Vue, Grammar::Svelte, Grammar::Astro] {
            assert!(
                std::ptr::eq(component.compiled().unwrap(), ts),
                "{component:?} should share TypeScript's cached query"
            );
        }
        assert!(!std::ptr::eq(Grammar::Tsx.compiled().unwrap(), ts));

        // Reusing one `Parsers` across languages (including embedded SQL switches)
        // must produce identical definitions and references to fresh instances.
        let ts_src =
            "export function fetchOrders() {\n  return \"SELECT id FROM app.orders\";\n}\n";
        let py_src = "def total(items):\n    return sum(items)\n";
        let mut shared = Parsers::default();
        let shared_ts = shared.parse(Grammar::TypeScript, ts_src).unwrap();
        let shared_py = shared.parse(Grammar::Python, py_src).unwrap();
        let shared_ts2 = shared.parse(Grammar::TypeScript, ts_src).unwrap();

        let fresh_ts = Parsers::default()
            .parse(Grammar::TypeScript, ts_src)
            .unwrap();
        let fresh_py = Parsers::default().parse(Grammar::Python, py_src).unwrap();

        assert_eq!(shared_ts.definitions.len(), fresh_ts.definitions.len());
        assert_eq!(shared_ts.references.len(), fresh_ts.references.len());
        assert_eq!(shared_ts2.definitions.len(), fresh_ts.definitions.len());
        assert_eq!(shared_ts2.references.len(), fresh_ts.references.len());
        assert_eq!(shared_py.definitions.len(), fresh_py.definitions.len());
        assert!(
            shared_ts
                .references
                .iter()
                .any(|r| r.kind == Kind::Data && r.name == "app.orders")
        );
    }

    #[test]
    fn js_ts_barrel_reexports_dynamic_imports_require_and_alias_stems() {
        let mut parsers = Parsers::default();
        let src = concat!(
            "import DefaultAuth, { verify as checkAuth } from './auth/index.ts';\n",
            "export { foo } from './orders';\n",
            "export { bar as baz } from './billing/index';\n",
            "export * from './users';\n",
            "export * as shared from '../shared/index.js';\n",
            "import { helper } from '@/foo/bar/index.ts';\n",
            "const { qux, legacy: oldQux } = require('./qux');\n",
            "export async function load() {\n",
            "  const mod = await import('./lazy');\n",
            "  return [DefaultAuth, checkAuth, helper, qux, oldQux, mod];\n",
            "}\n",
        );
        for grammar in [Grammar::TypeScript, Grammar::Tsx] {
            let facts = parsers.parse(grammar, src).unwrap();
            let paths: Vec<_> = facts.imports.iter().map(|i| i.path.as_str()).collect();
            assert_eq!(
                paths,
                [
                    "./auth",
                    "./orders",
                    "./billing",
                    "./users",
                    "../shared",
                    "@/foo/bar",
                    "./qux",
                    "./lazy",
                ],
                "{grammar:?}"
            );
            let stems: Vec<_> = facts.imports.iter().map(Import::module_stem).collect();
            assert_eq!(
                stems,
                [
                    "auth",
                    "orders",
                    "billing",
                    "users",
                    "../shared",
                    "foo/bar",
                    "qux",
                    "lazy",
                ],
                "{grammar:?}"
            );
            assert_eq!(
                facts.imports[0]
                    .bindings
                    .get("DefaultAuth")
                    .map(String::as_str),
                Some("DefaultAuth")
            );
            assert_eq!(
                facts.imports[0]
                    .bindings
                    .get("checkAuth")
                    .map(String::as_str),
                Some("verify")
            );
            assert_eq!(
                facts.imports[1].bindings.get("foo").map(String::as_str),
                Some("foo")
            );
            assert_eq!(
                facts.imports[2].bindings.get("baz").map(String::as_str),
                Some("bar")
            );
            assert!(facts.imports[3].bindings.is_empty());
            assert_eq!(
                facts.imports[6].bindings.get("qux").map(String::as_str),
                Some("qux")
            );
            assert_eq!(
                facts.imports[6].bindings.get("oldQux").map(String::as_str),
                Some("legacy")
            );
        }

        // Plain JS also extracts barrel re-exports, dynamic import, and require.
        let js_src = concat!(
            "export { foo } from './orders';\n",
            "export * from './users/index.js';\n",
            "const lazy = () => import('./lazy');\n",
            "const qux = require('./qux/index');\n",
        );
        let js_facts = parsers.parse(Grammar::JavaScript, js_src).unwrap();
        let js_paths: Vec<_> = js_facts.imports.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(js_paths, ["./orders", "./users", "./lazy", "./qux"]);
    }

    #[test]
    fn csharp_records_inheritance_attributes_and_qualified_receiver_calls() {
        let mut parsers = Parsers::default();
        let cs = concat!(
            "namespace App.Billing\n",
            "{\n",
            "    public interface IBillable {}\n",
            "    public record BaseEntity(int Id);\n",
            "    public record Invoice(int Id, decimal Total) : BaseEntity(Id), App.Billing.IBillable;\n",
            "    public record struct Money(decimal Amount) : System.IComparable<Money>;\n",
            "    [App.Security.Authorize]\n",
            "    public class CheckoutHandler : BaseHandler, IHandler<Invoice>\n",
            "    {\n",
            "        public CheckoutHandler() {}\n",
            "        [Xunit.Fact]\n",
            "        public void Handle(string payload)\n",
            "        {\n",
            "            var inv = System.Text.Json.JsonSerializer.Deserialize<Invoice>(payload);\n",
            "            App.Billing.InvoiceService.Charge(inv);\n",
            "            var svc = new App.Billing.AuditLogger();\n",
            "            svc?.Record(inv);\n",
            "        }\n",
            "    }\n",
            "}\n",
        );
        let facts = parsers.parse(Grammar::CSharp, cs).unwrap();
        let defs: Vec<_> = facts
            .definitions
            .iter()
            .map(|d| (d.kind.as_str(), d.name.as_str(), d.test))
            .collect();
        assert!(defs.contains(&("module", "App.Billing", false)), "{defs:?}");
        assert!(
            defs.contains(&("interface", "IBillable", false)),
            "{defs:?}"
        );
        assert!(defs.contains(&("record", "BaseEntity", false)), "{defs:?}");
        assert!(defs.contains(&("record", "Invoice", false)), "{defs:?}");
        assert!(defs.contains(&("record", "Money", false)), "{defs:?}");
        assert!(
            defs.contains(&("class", "CheckoutHandler", false)),
            "{defs:?}"
        );
        assert!(defs.contains(&("constructor", "new", false)), "{defs:?}");
        assert!(defs.contains(&("method", "Handle", true)), "{defs:?}");

        let refs: Vec<_> = facts
            .references
            .iter()
            .map(|r| (r.kind, r.name.as_str()))
            .collect();
        // Qualified receiver + method tokens preserved without losing either.
        assert!(
            refs.contains(&(Kind::Reference, "JsonSerializer")),
            "{refs:?}"
        );
        assert!(refs.contains(&(Kind::Call, "Deserialize")), "{refs:?}");
        assert!(
            refs.contains(&(Kind::Reference, "InvoiceService")),
            "{refs:?}"
        );
        assert!(refs.contains(&(Kind::Call, "Charge")), "{refs:?}");
        assert!(refs.contains(&(Kind::Call, "Record")), "{refs:?}");
        // Qualified object creation and attributes.
        assert!(refs.contains(&(Kind::Reference, "AuditLogger")), "{refs:?}");
        assert!(refs.contains(&(Kind::Reference, "Authorize")), "{refs:?}");
        // Clean base-class and interface inheritance without `<T>` or namespace noise.
        assert!(refs.contains(&(Kind::Reference, "BaseEntity")), "{refs:?}");
        assert!(refs.contains(&(Kind::Reference, "IBillable")), "{refs:?}");
        assert!(refs.contains(&(Kind::Reference, "IComparable")), "{refs:?}");
        assert!(refs.contains(&(Kind::Reference, "BaseHandler")), "{refs:?}");
        assert!(refs.contains(&(Kind::Reference, "IHandler")), "{refs:?}");
        assert!(
            !refs
                .iter()
                .any(|(_, name)| name.contains('<') || name.contains('.')),
            "inheritance/qualified refs should emit clean identifiers: {refs:?}"
        );
    }

    #[test]
    fn sql_dollar_quoted_functions_and_procedures_keep_full_span_and_body_tables() {
        let mut parsers = Parsers::default();
        let sql = concat!(
            "CREATE OR REPLACE FUNCTION app.reconcile_orders(p_since TIMESTAMPTZ)\n",
            "RETURNS INT\n",
            "AS $$\n",
            "DECLARE\n",
            "  v_count INT := 0;\n",
            "BEGIN\n",
            "  -- FROM ignored_comment_table\n",
            "  WITH active_ids AS (SELECT id FROM app.orders WHERE status = 'open')\n",
            "  SELECT COUNT(*) INTO v_count FROM active_ids;\n",
            "  IF v_count > 0 THEN\n",
            "    UPDATE app.orders SET status = 'reconciled' WHERE created_at >= p_since;\n",
            "    INSERT INTO app.audit_log(order_id, note)\n",
            "      SELECT o.id, 'SELECT * FROM ignored_string_table'\n",
            "      FROM app.orders o\n",
            "      JOIN app.customers c ON c.id = o.customer_id;\n",
            "  END IF;\n",
            "  DELETE FROM app.pending_queue WHERE processed = TRUE;\n",
            "  PERFORM EXTRACT(EPOCH FROM NOW());\n",
            "  RETURN v_count;\n",
            "END;\n",
            "$$ LANGUAGE plpgsql;\n\n",
            "CREATE OR REPLACE PROCEDURE app.archive_batch(p_id INT)\n",
            "AS $body$\n",
            "BEGIN\n",
            "  INSERT INTO app.order_archive SELECT * FROM app.orders WHERE id = p_id FOR UPDATE;\n",
            "  DELETE FROM app.orders WHERE id = p_id;\n",
            "END;\n",
            "$body$ LANGUAGE plpgsql;\n\n",
            "CREATE TABLE app.after_routines (\n",
            "  id INT PRIMARY KEY\n",
            ");\n",
        );
        let facts = parsers.parse(Grammar::Sql, sql).unwrap();
        let fn_idx = facts
            .definitions
            .iter()
            .position(|d| d.kind == "function" && d.name == "app.reconcile_orders")
            .expect("missing app.reconcile_orders");
        let fn_def = &facts.definitions[fn_idx];
        assert_eq!(fn_def.start, 1);
        assert_eq!(fn_def.end, 21);
        let fn_slice = &sql[fn_def.bytes.clone()];
        assert!(
            fn_slice.starts_with("CREATE OR REPLACE FUNCTION app.reconcile_orders")
                && fn_slice.ends_with("$$ LANGUAGE plpgsql;"),
            "function span cut off prematurely: {fn_slice:?}"
        );

        let proc_idx = facts
            .definitions
            .iter()
            .position(|d| d.kind == "procedure" && d.name == "app.archive_batch")
            .expect("missing app.archive_batch");
        let proc_def = &facts.definitions[proc_idx];
        assert_eq!(proc_def.start, 23);
        assert_eq!(proc_def.end, 29);
        assert!(sql[proc_def.bytes.clone()].ends_with("$body$ LANGUAGE plpgsql;"));

        // Trailing DDL after PL/pgSQL dollar-quoted routines must still parse cleanly.
        assert!(
            facts
                .definitions
                .iter()
                .any(|d| d.kind == "table" && d.name == "app.after_routines"),
            "trailing CREATE TABLE lost after PL/pgSQL routine: {:?}",
            facts.definitions
        );

        let fn_tables: Vec<_> = facts
            .references
            .iter()
            .filter(|r| r.kind == Kind::Data && r.from == Some(fn_idx))
            .map(|r| r.name.as_str())
            .collect();
        for expected in [
            "app.orders",
            "app.audit_log",
            "app.customers",
            "app.pending_queue",
        ] {
            assert!(
                fn_tables.contains(&expected),
                "missing {expected} in {fn_tables:?}"
            );
        }
        for forbidden in [
            "ignored_comment_table",
            "ignored_string_table",
            "active_ids",
            "now",
        ] {
            assert!(
                !fn_tables.contains(&forbidden),
                "unexpected false-positive table {forbidden} in {fn_tables:?}"
            );
        }

        let proc_tables: Vec<_> = facts
            .references
            .iter()
            .filter(|r| r.kind == Kind::Data && r.from == Some(proc_idx))
            .map(|r| r.name.as_str())
            .collect();
        assert!(
            proc_tables.contains(&"app.order_archive"),
            "{proc_tables:?}"
        );
        assert!(proc_tables.contains(&"app.orders"), "{proc_tables:?}");
    }

    #[test]
    fn fixtures_mixed_polyglot_extraction_remains_intact() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/mixed");
        let mut parsers = Parsers::default();

        let orders_ts = std::fs::read_to_string(root.join("frontend/orders.ts")).unwrap();
        let ts_facts = parsers.parse(Grammar::TypeScript, &orders_ts).unwrap();
        assert!(ts_facts.definitions.iter().any(|d| d.name == "fetchOrders"));
        assert_eq!(ts_facts.imports.len(), 1);
        assert_eq!(ts_facts.imports[0].path, "./format.js");
        assert_eq!(ts_facts.imports[0].module_stem(), "format");
        assert_eq!(
            ts_facts.imports[0]
                .bindings
                .get("formatOrder")
                .map(String::as_str),
            Some("formatOrder")
        );
        assert!(
            ts_facts
                .references
                .iter()
                .any(|r| r.kind == Kind::Route && r.name == "/api/orders")
        );

        let orders_py = std::fs::read_to_string(root.join("backend/orders.py")).unwrap();
        let py_facts = parsers.parse(Grammar::Python, &orders_py).unwrap();
        assert!(py_facts.routes.iter().any(|(r, _)| r == "/api/orders"));
        assert!(
            py_facts
                .references
                .iter()
                .any(|r| r.kind == Kind::Data && r.name == "purchase_orders")
        );

        let schema_sql = std::fs::read_to_string(root.join("db/schema.sql")).unwrap();
        let sql_facts = parsers.parse(Grammar::Sql, &schema_sql).unwrap();
        assert!(
            sql_facts
                .definitions
                .iter()
                .any(|d| d.kind == "table" && d.name == "purchase_orders")
        );
    }

    #[test]
    fn fast_path_normalizers_handle_case_insensitive_extensions_and_sql_names() {
        assert_eq!(Grammar::for_file("App.TSX"), Some(Grammar::Tsx));
        assert_eq!(Grammar::for_file("Program.CS"), Some(Grammar::CSharp));
        assert_eq!(Grammar::for_file("schema.SQL"), Some(Grammar::Sql));
        assert_eq!(Grammar::for_file("archive.UNKNOWNEXTLONG"), None);
        assert!(matches!(
            sql_name_cow("app.orders"),
            Cow::Borrowed("app.orders")
        ));
        assert_eq!(sql_name_cow("\"App\".\"Orders\"").as_ref(), "app.orders");
        assert_eq!(normalize_js_module_stem("./foo/index.tsx"), "foo");
        assert_eq!(
            normalize_js_module_stem("@/billing/orders/index"),
            "billing/orders"
        );
    }

    #[test]
    fn sql_dollar_quoted_body_handles_non_ascii_utf8_identifiers_and_punctuation() {
        let mut parsers = Parsers::default();
        let sql = concat!(
            "CREATE OR REPLACE FUNCTION app.sync_café(p_note text) RETURNS void AS $$\n",
            "BEGIN\n",
            "  SELECT café\u{00a0}— «€» FROM app.orders;\n",
            "  INSERT INTO app.audit_log(über_col) VALUES (café);\n",
            "END;\n",
            "$$ LANGUAGE plpgsql;\n",
        );
        let facts = parsers.parse(Grammar::Sql, sql).unwrap();
        let fn_idx = facts
            .definitions
            .iter()
            .position(|d| d.kind == "function" && d.name.starts_with("app.sync_caf"))
            .expect("dollar-quoted routine should be extracted without panicking");
        let tables: Vec<_> = facts
            .references
            .iter()
            .filter(|r| r.kind == Kind::Data && r.from == Some(fn_idx))
            .map(|r| r.name.as_str())
            .collect();
        assert!(tables.contains(&"app.orders"), "{tables:?}");
        assert!(tables.contains(&"app.audit_log"), "{tables:?}");
    }
}
