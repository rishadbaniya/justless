use std::collections::BTreeMap;

use proc_macro2::Span;
use serde::{Deserialize, Serialize};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{Expr, Item, Pat, Token, Type, UseTree};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Symbol {
    #[serde(skip)]
    pub bytes: std::ops::Range<usize>,
    pub name: String,
    pub path: Vec<String>,
    pub kind: String,
    pub start: usize,
    pub end: usize,
    pub declaration: usize,
    pub test: bool,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum Kind {
    Call,
    Method,
    Reference,
    Import,
    Route,
    Data,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reference {
    pub path: Vec<String>,
    pub module: Vec<String>,
    pub from: Option<usize>,
    pub kind: Kind,
    pub line: usize,
    pub inferred_receiver: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Import {
    pub module: Vec<String>,
    pub from: Option<usize>,
    pub alias: Option<String>,
    pub path: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModuleFile {
    pub path: Vec<String>,
    pub override_path: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Syntax {
    #[serde(skip)]
    pub use_ranges: Vec<std::ops::Range<usize>>,
    pub symbols: Vec<Symbol>,
    pub references: Vec<Reference>,
    pub imports: Vec<Import>,
    pub modules: Vec<ModuleFile>,
}

/// Delimiter nesting or a run of operators longer than this is reported as a
/// parse error instead of recursing through syn. rustc itself stops compiling
/// before 3,000 nested parentheses.
const MAX_NESTING: usize = 1024;

/// `syn::parse_file`, with a nesting check on the tokens before syn recurses
/// through them.
pub(crate) fn parse_file(text: &str) -> syn::Result<syn::File> {
    use proc_macro2::TokenTree;
    let mut content = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut shebang = None;
    if content.starts_with("#!") && !skip_trivia(&content[2..]).starts_with('[') {
        let end = content.find('\n').unwrap_or(content.len());
        shebang = Some(content[..end].to_owned());
        content = &content[end..];
    }
    let tokens: proc_macro2::TokenStream = content.parse()?;
    let mut levels = vec![tokens.clone().into_iter()];
    // Prefix operators such as `!!!x` nest without delimiters.
    let mut operators = 0;
    while let Some(level) = levels.last_mut() {
        let token = level.next();
        operators = match &token {
            Some(TokenTree::Punct(_)) => operators + 1,
            _ => 0,
        };
        let too_deep = match token {
            Some(TokenTree::Group(group)) if levels.len() > MAX_NESTING => Some(group.span_open()),
            Some(TokenTree::Group(group)) => {
                levels.push(group.stream().into_iter());
                None
            }
            Some(punct) if operators > MAX_NESTING => Some(punct.span()),
            Some(_) => None,
            None => {
                levels.pop();
                None
            }
        };
        if let Some(span) = too_deep {
            return Err(syn::Error::new(
                span,
                format!("nesting deeper than {MAX_NESTING} levels"),
            ));
        }
    }
    let mut file: syn::File = syn::parse2(tokens)?;
    file.shebang = shebang;
    Ok(file)
}

/// Whitespace and non-doc comments, as `syn::parse_file` skips them to tell a
/// shebang from an inner attribute.
fn skip_trivia(mut s: &str) -> &str {
    loop {
        let trimmed =
            s.trim_start_matches(|c: char| c.is_whitespace() || c == '\u{200e}' || c == '\u{200f}');
        if trimmed.starts_with("//")
            && (!trimmed.starts_with("///") || trimmed.starts_with("////"))
            && !trimmed.starts_with("//!")
        {
            s = trimmed.find('\n').map_or("", |end| &trimmed[end + 1..]);
        } else if let Some(rest) = trimmed.strip_prefix("/**/") {
            s = rest;
        } else if trimmed.starts_with("/*")
            && (!trimmed.starts_with("/**") || trimmed.starts_with("/***"))
            && !trimmed.starts_with("/*!")
        {
            let bytes = trimmed.as_bytes();
            let (mut depth, mut i) = (0, 0);
            loop {
                match bytes.get(i..i + 2) {
                    Some(b"/*") => depth += 1,
                    Some(b"*/") => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    Some(_) => {
                        i += 1;
                        continue;
                    }
                    None => return trimmed,
                }
                i += 2;
            }
            s = &trimmed[i + 2..];
        } else {
            return trimmed;
        }
    }
}

pub fn parse(text: &str) -> syn::Result<Syntax> {
    let file = parse_file(text)?;
    let mut visitor = Extractor::default();
    visitor.visit_file(&file);
    let offset = usize::from(text.starts_with('\u{feff}')) * '\u{feff}'.len_utf8()
        + file.shebang.as_ref().map_or(0, String::len);
    if offset > 0 {
        for range in visitor
            .syntax
            .use_ranges
            .iter_mut()
            .chain(visitor.syntax.symbols.iter_mut().map(|s| &mut s.bytes))
        {
            range.start += offset;
            range.end += offset;
        }
    }
    Ok(visitor.syntax)
}

#[derive(Default)]
struct Extractor {
    syntax: Syntax,
    module: Vec<String>,
    symbol: Option<usize>,
    owner: Option<Vec<String>>,
    locals: Vec<BTreeMap<String, Option<Vec<String>>>>,
}

impl Extractor {
    fn with_symbol(
        &mut self,
        ident: &syn::Ident,
        kind: &str,
        span: Span,
        test: bool,
        walk: impl FnOnce(&mut Self),
    ) {
        let name = identifier(ident);
        let mut path = self.module.clone();
        if matches!(kind, "method" | "associated type" | "associated constant")
            && let Some(owner) = &self.owner
            && let Some(name) = owner.last()
        {
            path.push(name.clone());
        } else if kind == "function"
            && let Some(parent) = self.symbol
            && self.syntax.symbols[parent].kind != "module"
        {
            path = self.syntax.symbols[parent].path.clone();
        }
        path.push(name.clone());
        let start = span.start().line;
        let end = span.end();
        let end = if end.column == 0 && end.line > start {
            end.line - 1
        } else {
            end.line
        };
        let previous = self.symbol.replace(self.syntax.symbols.len());
        self.syntax.symbols.push(Symbol {
            bytes: span.byte_range(),
            name,
            path,
            kind: kind.to_owned(),
            start,
            end,
            declaration: ident.span().start().line,
            test,
        });
        walk(self);
        self.symbol = previous;
    }

    fn reference(
        &mut self,
        mut path: Vec<String>,
        kind: Kind,
        span: Span,
        inferred_receiver: bool,
    ) {
        if path.is_empty() {
            return;
        }
        if path[0] == "Self" {
            let Some(owner) = &self.owner else { return };
            path.splice(..1, owner.clone());
        } else if path.len() == 1
            && matches!(kind, Kind::Call | Kind::Reference)
            && self.local(&path[0]).is_some()
        {
            return;
        }
        self.syntax.references.push(Reference {
            path,
            module: self.module.clone(),
            from: self.symbol,
            kind,
            line: span.start().line,
            inferred_receiver,
        });
    }

    fn local(&self, name: &str) -> Option<&Option<Vec<String>>> {
        self.locals.iter().rev().find_map(|scope| scope.get(name))
    }

    fn bind(&mut self, pattern: &Pat, ty: Option<Vec<String>>) {
        match pattern {
            Pat::Ident(binding) => {
                if let Some(scope) = self.locals.last_mut() {
                    scope.insert(identifier(&binding.ident), ty.clone());
                }
                if let Some((_, pattern)) = &binding.subpat {
                    self.bind(pattern, ty);
                }
            }
            Pat::Type(annotated) => self.bind(&annotated.pat, type_path(&annotated.ty)),
            Pat::Tuple(tuple) => tuple.elems.iter().for_each(|item| self.bind(item, None)),
            Pat::TupleStruct(tuple) => tuple.elems.iter().for_each(|item| self.bind(item, None)),
            Pat::Struct(record) => record
                .fields
                .iter()
                .for_each(|field| self.bind(&field.pat, None)),
            Pat::Slice(slice) => slice.elems.iter().for_each(|item| self.bind(item, None)),
            Pat::Or(alternatives) => alternatives
                .cases
                .iter()
                .for_each(|item| self.bind(item, None)),
            Pat::Reference(reference) => self.bind(&reference.pat, ty),
            Pat::Paren(paren) => self.bind(&paren.pat, ty),
            _ => {}
        }
    }

    fn signature(&mut self, signature: &syn::Signature, walk: impl FnOnce(&mut Self)) {
        self.locals.push(BTreeMap::new());
        for parameter in &signature.inputs {
            if let syn::FnArg::Typed(parameter) = parameter {
                self.bind(&parameter.pat, type_path(&parameter.ty));
            }
        }
        walk(self);
        self.locals.pop();
    }

    fn imports(&mut self, tree: &UseTree, mut prefix: Vec<String>, span: Span) {
        let (alias, path) = match tree {
            UseTree::Path(part) => {
                prefix.push(identifier(&part.ident));
                return self.imports(&part.tree, prefix, span);
            }
            UseTree::Group(group) => {
                for item in &group.items {
                    self.imports(item, prefix.clone(), item.span());
                }
                return;
            }
            UseTree::Glob(_) => (None, prefix),
            UseTree::Name(name) => {
                let name = identifier(&name.ident);
                let alias = if name == "self" {
                    prefix.last().cloned()
                } else {
                    prefix.push(name.clone());
                    Some(name)
                };
                (alias, prefix)
            }
            UseTree::Rename(rename) => {
                if rename.ident != "self" {
                    prefix.push(identifier(&rename.ident));
                }
                (Some(identifier(&rename.rename)), prefix)
            }
        };
        if alias.is_some() {
            self.reference(path.clone(), Kind::Import, span, false);
        }
        self.syntax.imports.push(Import {
            module: self.module.clone(),
            from: self.symbol,
            alias,
            path,
        });
    }
}

impl<'ast> Visit<'ast> for Extractor {
    fn visit_item(&mut self, item: &'ast Item) {
        match item {
            Item::Fn(function) => self.with_symbol(
                &function.sig.ident,
                "function",
                item.span(),
                is_test(&function.attrs),
                |this| this.signature(&function.sig, |this| visit::visit_item_fn(this, function)),
            ),
            Item::Mod(module) => {
                self.with_symbol(&module.ident, "module", item.span(), false, |this| {
                    this.module.push(identifier(&module.ident));
                    if module.content.is_none() {
                        let override_path = module.attrs.iter().find_map(|attribute| {
                            if attribute.path().is_ident("path")
                                && let syn::Meta::NameValue(meta) = &attribute.meta
                                && let Expr::Lit(literal) = &meta.value
                                && let syn::Lit::Str(path) = &literal.lit
                            {
                                return Some(path.value());
                            }
                            None
                        });
                        this.syntax.modules.push(ModuleFile {
                            path: this.module.clone(),
                            override_path,
                        });
                    }
                    visit::visit_item_mod(this, module);
                    this.module.pop();
                })
            }
            Item::Impl(implementation) => {
                if let Some((_, path, _)) = &implementation.trait_ {
                    self.reference(path_parts(path), Kind::Reference, path.span(), false);
                }
                let previous =
                    std::mem::replace(&mut self.owner, type_path(&implementation.self_ty));
                visit::visit_item_impl(self, implementation);
                self.owner = previous;
            }
            Item::Struct(value) => {
                self.with_symbol(&value.ident, "struct", item.span(), false, |this| {
                    visit::visit_item_struct(this, value)
                })
            }
            Item::Enum(value) => {
                self.with_symbol(&value.ident, "enum", item.span(), false, |this| {
                    visit::visit_item_enum(this, value)
                })
            }
            Item::Trait(value) => {
                self.with_symbol(&value.ident, "trait", item.span(), false, |this| {
                    let previous = this.owner.replace(vec![identifier(&value.ident)]);
                    visit::visit_item_trait(this, value);
                    this.owner = previous;
                })
            }
            Item::Type(value) => {
                self.with_symbol(&value.ident, "type", item.span(), false, |this| {
                    visit::visit_item_type(this, value)
                })
            }
            Item::Const(value) => {
                self.with_symbol(&value.ident, "constant", item.span(), false, |this| {
                    visit::visit_item_const(this, value)
                })
            }
            Item::Static(value) => {
                self.with_symbol(&value.ident, "static", item.span(), false, |this| {
                    visit::visit_item_static(this, value)
                })
            }
            Item::Use(value) => {
                self.syntax.use_ranges.push(value.span().byte_range());
                self.imports(&value.tree, Vec::new(), value.tree.span());
            }
            _ => visit::visit_item(self, item),
        }
    }

    fn visit_impl_item_fn(&mut self, method: &'ast syn::ImplItemFn) {
        self.with_symbol(
            &method.sig.ident,
            "method",
            method.span(),
            is_test(&method.attrs),
            |this| {
                this.signature(&method.sig, |this| visit::visit_impl_item_fn(this, method));
            },
        );
    }

    fn visit_trait_item_fn(&mut self, method: &'ast syn::TraitItemFn) {
        self.with_symbol(&method.sig.ident, "method", method.span(), false, |this| {
            this.signature(&method.sig, |this| visit::visit_trait_item_fn(this, method));
        });
    }

    fn visit_block(&mut self, block: &'ast syn::Block) {
        self.locals.push(BTreeMap::new());
        visit::visit_block(self, block);
        self.locals.pop();
    }

    fn visit_local(&mut self, local: &'ast syn::Local) {
        visit::visit_local(self, local);
        let inferred = local.init.as_ref().and_then(|init| {
            if let Expr::Struct(value) = init.expr.as_ref() {
                Some(path_parts(&value.path))
            } else {
                None
            }
        });
        self.bind(&local.pat, inferred);
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let Expr::Path(path) = call.func.as_ref()
            && path.qself.is_none()
        {
            self.reference(path_parts(&path.path), Kind::Call, path.span(), false);
        } else {
            self.visit_expr(&call.func);
        }
        for arg in &call.args {
            self.visit_expr(arg);
        }
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        let receiver = match call.receiver.as_ref() {
            Expr::Path(path) if path.path.is_ident("self") => self.owner.clone(),
            Expr::Path(path) => path
                .path
                .get_ident()
                .and_then(|ident| self.local(&identifier(ident)))
                .cloned()
                .flatten(),
            Expr::Struct(value) => Some(path_parts(&value.path)),
            _ => None,
        };
        let inferred = receiver.is_none();
        let mut path = receiver.unwrap_or_default();
        path.push(identifier(&call.method));
        self.reference(path, Kind::Method, call.method.span(), inferred);
        visit::visit_expr_method_call(self, call);
    }

    fn visit_expr_path(&mut self, path: &'ast syn::ExprPath) {
        if path.qself.is_none() {
            self.reference(path_parts(&path.path), Kind::Reference, path.span(), false);
        }
        visit::visit_expr_path(self, path);
    }

    fn visit_type_path(&mut self, path: &'ast syn::TypePath) {
        if path.qself.is_none() {
            self.reference(path_parts(&path.path), Kind::Reference, path.span(), false);
        }
        visit::visit_type_path(self, path);
    }

    // Bounds in generics and where clauses, supertraits, `dyn Trait` and `impl Trait`.
    fn visit_trait_bound(&mut self, bound: &'ast syn::TraitBound) {
        self.reference(
            path_parts(&bound.path),
            Kind::Reference,
            bound.path.span(),
            false,
        );
        visit::visit_trait_bound(self, bound);
    }

    // Variant and struct patterns, such as `Status::Open(code)` or `Point { x, .. }`.
    // Plain path patterns already arrive through visit_expr_path.
    fn visit_pat_tuple_struct(&mut self, pattern: &'ast syn::PatTupleStruct) {
        if pattern.qself.is_none() {
            self.reference(
                path_parts(&pattern.path),
                Kind::Reference,
                pattern.path.span(),
                false,
            );
        }
        visit::visit_pat_tuple_struct(self, pattern);
    }

    fn visit_pat_struct(&mut self, pattern: &'ast syn::PatStruct) {
        if pattern.qself.is_none() {
            self.reference(
                path_parts(&pattern.path),
                Kind::Reference,
                pattern.path.span(),
                false,
            );
        }
        visit::visit_pat_struct(self, pattern);
    }

    fn visit_expr_struct(&mut self, value: &'ast syn::ExprStruct) {
        self.reference(
            path_parts(&value.path),
            Kind::Reference,
            value.path.span(),
            false,
        );
        visit::visit_expr_struct(self, value);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if mac.path.segments.last().is_some_and(|segment| {
            matches!(
                segment.ident.to_string().as_str(),
                "assert"
                    | "assert_eq"
                    | "assert_ne"
                    | "debug_assert"
                    | "debug_assert_eq"
                    | "debug_assert_ne"
                    | "format"
                    | "format_args"
                    | "write"
                    | "writeln"
                    | "unreachable"
                    | "todo"
                    | "unimplemented"
                    | "print"
                    | "println"
                    | "eprint"
                    | "eprintln"
                    | "panic"
                    | "dbg"
                    | "vec"
                    | "matches"
            )
        }) && let Ok(expressions) =
            mac.parse_body_with(Punctuated::<Expr, Token![,]>::parse_terminated)
        {
            for expression in &expressions {
                self.visit_expr(expression);
            }
        }
    }
}

fn identifier(ident: &syn::Ident) -> String {
    let name = ident.to_string();
    name.strip_prefix("r#").unwrap_or(&name).to_owned()
}

fn path_parts(path: &syn::Path) -> Vec<String> {
    path.segments
        .iter()
        .map(|segment| identifier(&segment.ident))
        .collect()
}

fn type_path(ty: &Type) -> Option<Vec<String>> {
    match ty {
        Type::Path(path) if path.qself.is_none() => Some(path_parts(&path.path)),
        Type::Reference(reference) => type_path(&reference.elem),
        Type::Paren(paren) => type_path(&paren.elem),
        _ => None,
    }
}

fn is_test(attributes: &[syn::Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        attribute.path().segments.last().is_some_and(|segment| {
            matches!(
                segment.ident.to_string().as_str(),
                "test" | "rstest" | "test_case"
            )
        })
    })
}
