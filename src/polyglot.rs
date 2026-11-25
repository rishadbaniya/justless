use std::collections::{BTreeMap, BTreeSet};
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
            Self::CSharp => tree_sitter_c_sharp::TAGS_QUERY,
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

struct RawTag<'a> {
    name: &'a str,
    node: Node<'a>,
    name_node: Node<'a>,
    label: &'static str,
}

struct CompiledQuery {
    language: Language,
    query: Query,
    name_index: Option<u32>,
}

impl CompiledQuery {
    fn compile(grammar: Grammar) -> Result<Self, String> {
        let language = grammar.language();
        let tags = grammar.tags_source();
        let query = Query::new(&language, &tags).map_err(|error| error.to_string())?;
        let name_index = query.capture_index_for_name("name");
        Ok(Self {
            language,
            query,
            name_index,
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
        let tree = parser
            .parse(source, None)
            .ok_or_else(|| anyhow::anyhow!("parse cancelled"))?;
        let query = &compiled.query;
        let captures = query.capture_names();
        let mut definitions = Vec::new();
        let mut references = Vec::new();
        if let Some(name_index) = compiled.name_index {
            let cursor = self.cursor.get_or_insert_with(QueryCursor::new);
            let mut matches = cursor.matches(query, tree.root_node(), source.as_bytes());
            while let Some(matched) = matches.next() {
                let Some(name) = matched.captures.iter().find(|cap| cap.index == name_index) else {
                    continue;
                };
                let name_text = text(name.node, source);
                if name_text.is_empty() {
                    continue;
                }
                for capture in matched.captures {
                    let label = captures[capture.index as usize];
                    if label.starts_with("definition.") {
                        definitions.push(RawTag {
                            name: name_text,
                            node: capture.node,
                            name_node: name.node,
                            label,
                        });
                    } else if label.starts_with("reference.") {
                        references.push(RawTag {
                            name: name_text,
                            node: capture.node,
                            name_node: name.node,
                            label,
                        });
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
                    || tag.label != "definition.test"
                    || literal(tag.name).is_some())
        });
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
            // Declarators that share one statement are siblings, not owners.
            let parent = (0..id).rev().find(|&parent| {
                let owner = definitions[parent].node;
                owner.end_byte() >= node.end_byte() && owner.byte_range() != node.byte_range()
            });
            let (name, kind) = match (grammar, label.strip_prefix("definition.").unwrap()) {
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
                (Grammar::Groovy, "constructor") => ("new".to_owned(), "constructor"),
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
        let mut seen = BTreeSet::new();
        // Where a call names the function it defines, as Elixir and Julia
        // heads do, the name is the definition, not a call to it.
        let declared: BTreeSet<_> = definitions
            .iter()
            .map(|tag| (tag.name_node.start_byte(), tag.name_node.end_byte()))
            .collect();
        for tag in references {
            if declared.contains(&(tag.name_node.start_byte(), tag.name_node.end_byte()))
                || (grammar == Grammar::Elixir && module_attribute(tag.node))
            {
                continue;
            }
            let kind = match tag.label {
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
                || import_like(grammar, node, source)
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
fn import_like(grammar: Grammar, node: Node<'_>, source: &str) -> bool {
    match (grammar, node.kind()) {
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
    } else if matches!(grammar, Grammar::C | Grammar::Cpp | Grammar::ObjectiveC)
        && node.kind() == "preproc_include"
    {
        literal(text(node.child_by_field_name("path")?, source))?
    } else if grammar == Grammar::Julia
        && node.kind() == "call_expression"
        && text(node.named_child(0)?, source) == "include"
    {
        let argument = node.named_child(1)?.named_child(0)?;
        (argument.kind() == "string_literal").then(|| literal(text(argument, source)))??
    } else {
        return None;
    };
    Some(Import {
        path: path.to_owned(),
        bindings,
        line: node.start_position().row + 1,
    })
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
}
