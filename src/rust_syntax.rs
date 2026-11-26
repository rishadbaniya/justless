use std::fmt::Write as _;

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
    let mut levels = Vec::with_capacity(16);
    levels.push(tokens.clone().into_iter());
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
    extract(text, false)
}

/// [`parse`], also reading the body of every macro call that parses as
/// comma-separated expressions, such as `ensure!(cond, "{}", detail())`. The
/// index does not, because a macro may give those tokens another meaning.
pub fn parse_macros(text: &str) -> syn::Result<Syntax> {
    extract(text, true)
}

fn extract(text: &str, every_macro: bool) -> syn::Result<Syntax> {
    let file = parse_file(text)?;
    let item_count = file.items.len();
    let mut visitor = Extractor {
        syntax: Syntax {
            use_ranges: Vec::new(),
            symbols: Vec::with_capacity(item_count),
            references: Vec::with_capacity(item_count.saturating_mul(4)),
            imports: Vec::new(),
            modules: Vec::new(),
        },
        locals: Vec::with_capacity(32),
        scope_starts: Vec::with_capacity(16),
        scratch: String::with_capacity(64),
        every_macro,
        ..Extractor::default()
    };
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

/// Where and why `text` fails to parse as a Rust file, as a byte range of `text`
/// and syn's message, or `None` when it parses.
pub fn first_error(text: &str) -> Option<(std::ops::Range<usize>, String)> {
    let error = parse_file(text).err()?;
    let content = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut offset = text.len() - content.len();
    if content.starts_with("#!") && !skip_trivia(&content[2..]).starts_with('[') {
        offset += content.find('\n').unwrap_or(content.len());
    }
    let span = error.span();
    // An error with no source location, such as the end of the input, has the
    // call-site span, which has no source text.
    let bytes = match span.source_text() {
        Some(_) => span.byte_range(),
        None => text.len() - offset..text.len() - offset,
    };
    let start = (bytes.start + offset).min(text.len());
    let end = (bytes.end + offset).clamp(start, text.len());
    let mut message = error.to_string();
    // The lexer names no cause, but it stops at the token it could not close.
    if message == "cannot parse string into token stream" {
        let rest = text.get(start..).unwrap_or_default();
        message = match rest.chars().next() {
            Some(open @ ('{' | '(' | '[')) => format!("unclosed `{open}`"),
            Some(close @ ('}' | ')' | ']')) => format!("unmatched `{close}`"),
            Some('"') => "unterminated string".into(),
            _ if rest.starts_with("/*") => "unterminated block comment".into(),
            _ => "invalid token".into(),
        };
    }
    Some((start..end, message))
}

#[derive(Default)]
struct Extractor {
    syntax: Syntax,
    module: Vec<String>,
    symbol: Option<usize>,
    owner: Option<Vec<String>>,
    /// Flat stack of local bindings across nested lexical scopes.
    locals: Vec<(String, Option<Vec<String>>)>,
    scope_starts: Vec<usize>,
    /// Start index in `locals` visible to the current item (nested items cannot
    /// capture outer function locals).
    locals_floor: usize,
    /// Reusable buffer for allocation-free `Ident` formatting during lookups.
    scratch: String,
    /// Whether to read the body of any macro call, not only the standard ones.
    every_macro: bool,
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
        let mut path = if matches!(kind, "method" | "associated type" | "associated constant")
            && let Some(owner) = &self.owner
            && let Some(owner_name) = owner.last()
        {
            let mut path = Vec::with_capacity(self.module.len() + 2);
            path.extend(self.module.iter().cloned());
            path.push(owner_name.clone());
            path
        } else if matches!(kind, "function" | "macro")
            && let Some(parent) = self.symbol
            && self.syntax.symbols[parent].kind != "module"
        {
            let parent_path = &self.syntax.symbols[parent].path;
            let mut path = Vec::with_capacity(parent_path.len() + 1);
            path.extend(parent_path.iter().cloned());
            path
        } else {
            let mut path = Vec::with_capacity(self.module.len() + 1);
            path.extend(self.module.iter().cloned());
            path
        };
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
            let mut resolved = Vec::with_capacity(owner.len() + path.len() - 1);
            resolved.extend(owner.iter().cloned());
            resolved.extend(path.into_iter().skip(1));
            path = resolved;
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

    /// Records a reference from a `syn::Path` without allocating when a single
    /// segment names a local binding or `Self` without an enclosing owner.
    fn reference_path(
        &mut self,
        path: &syn::Path,
        kind: Kind,
        span: Span,
        inferred_receiver: bool,
    ) {
        let Some(first) = path.segments.first() else {
            return;
        };
        let resolved = if first.ident == "Self" {
            let Some(owner) = &self.owner else { return };
            let mut resolved = Vec::with_capacity(owner.len() + path.segments.len() - 1);
            resolved.extend(owner.iter().cloned());
            resolved.extend(path.segments.iter().skip(1).map(|s| identifier(&s.ident)));
            resolved
        } else if path.segments.len() == 1 && matches!(kind, Kind::Call | Kind::Reference) {
            let name = ident_str(&first.ident, &mut self.scratch);
            if self.locals[self.locals_floor..]
                .iter()
                .rev()
                .any(|(local, _)| local == name)
            {
                return;
            }
            vec![name.to_owned()]
        } else {
            path_parts(path)
        };
        self.syntax.references.push(Reference {
            path: resolved,
            module: self.module.clone(),
            from: self.symbol,
            kind,
            line: span.start().line,
            inferred_receiver,
        });
    }

    fn local(&self, name: &str) -> Option<&Option<Vec<String>>> {
        self.locals[self.locals_floor..]
            .iter()
            .rev()
            .find(|(local, _)| local == name)
            .map(|(_, ty)| ty)
    }

    fn push_scope(&mut self) {
        self.scope_starts.push(self.locals.len());
    }

    fn pop_scope(&mut self) {
        let start = self.scope_starts.pop().unwrap_or(0);
        self.locals.truncate(start);
    }

    fn bind(&mut self, pattern: &Pat, ty: Option<Vec<String>>) {
        match pattern {
            Pat::Ident(binding) => {
                if !self.scope_starts.is_empty() {
                    let name = identifier(&binding.ident);
                    if let Some((_, subpat)) = &binding.subpat {
                        self.locals.push((name, ty.clone()));
                        self.bind(subpat, ty);
                    } else {
                        self.locals.push((name, ty));
                    }
                } else if let Some((_, subpat)) = &binding.subpat {
                    self.bind(subpat, ty);
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
        self.push_scope();
        for parameter in &signature.inputs {
            if let syn::FnArg::Typed(parameter) = parameter {
                self.bind(&parameter.pat, type_path(&parameter.ty));
            }
        }
        walk(self);
        self.pop_scope();
    }

    fn imports(&mut self, tree: &UseTree, prefix: &mut Vec<String>, span: Span) {
        let (alias, path) = match tree {
            UseTree::Path(part) => {
                let saved = prefix.len();
                prefix.push(identifier(&part.ident));
                self.imports(&part.tree, prefix, span);
                prefix.truncate(saved);
                return;
            }
            UseTree::Group(group) => {
                for item in &group.items {
                    self.imports(item, prefix, item.span());
                }
                return;
            }
            UseTree::Glob(_) => (None, prefix.clone()),
            UseTree::Name(name) => {
                let name = identifier(&name.ident);
                if name == "self" {
                    (prefix.last().cloned(), prefix.clone())
                } else {
                    let mut path = Vec::with_capacity(prefix.len() + 1);
                    path.extend(prefix.iter().cloned());
                    path.push(name.clone());
                    (Some(name), path)
                }
            }
            UseTree::Rename(rename) => {
                let path = if rename.ident == "self" {
                    prefix.clone()
                } else {
                    let mut path = Vec::with_capacity(prefix.len() + 1);
                    path.extend(prefix.iter().cloned());
                    path.push(identifier(&rename.ident));
                    path
                };
                (Some(identifier(&rename.rename)), path)
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
        let previous_floor = std::mem::replace(&mut self.locals_floor, self.locals.len());
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
                    let mod_name = this.syntax.symbols[this.symbol.unwrap()].name.clone();
                    this.module.push(mod_name);
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
                    self.reference_path(path, Kind::Reference, path.span(), false);
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
            Item::Union(value) => {
                self.with_symbol(&value.ident, "union", item.span(), false, |this| {
                    visit::visit_item_union(this, value)
                })
            }
            Item::Trait(value) => {
                self.with_symbol(&value.ident, "trait", item.span(), false, |this| {
                    let trait_name = this.syntax.symbols[this.symbol.unwrap()].name.clone();
                    let previous = this.owner.replace(vec![trait_name]);
                    visit::visit_item_trait(this, value);
                    this.owner = previous;
                })
            }
            Item::TraitAlias(value) => {
                self.with_symbol(&value.ident, "trait alias", item.span(), false, |this| {
                    visit::visit_item_trait_alias(this, value)
                })
            }
            Item::Macro(value) => {
                if let Some(ident) = &value.ident {
                    self.with_symbol(ident, "macro", item.span(), false, |this| {
                        visit::visit_item_macro(this, value)
                    });
                } else {
                    visit::visit_item_macro(self, value);
                }
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
                let mut prefix = Vec::with_capacity(4);
                self.imports(&value.tree, &mut prefix, value.tree.span());
            }
            _ => visit::visit_item(self, item),
        }
        self.locals_floor = previous_floor;
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

    fn visit_impl_item_type(&mut self, item: &'ast syn::ImplItemType) {
        self.with_symbol(&item.ident, "associated type", item.span(), false, |this| {
            visit::visit_impl_item_type(this, item)
        });
    }

    fn visit_impl_item_const(&mut self, item: &'ast syn::ImplItemConst) {
        self.with_symbol(
            &item.ident,
            "associated constant",
            item.span(),
            false,
            |this| visit::visit_impl_item_const(this, item),
        );
    }

    fn visit_trait_item_fn(&mut self, method: &'ast syn::TraitItemFn) {
        self.with_symbol(&method.sig.ident, "method", method.span(), false, |this| {
            this.signature(&method.sig, |this| visit::visit_trait_item_fn(this, method));
        });
    }

    fn visit_trait_item_type(&mut self, item: &'ast syn::TraitItemType) {
        self.with_symbol(&item.ident, "associated type", item.span(), false, |this| {
            visit::visit_trait_item_type(this, item)
        });
    }

    fn visit_trait_item_const(&mut self, item: &'ast syn::TraitItemConst) {
        self.with_symbol(
            &item.ident,
            "associated constant",
            item.span(),
            false,
            |this| visit::visit_trait_item_const(this, item),
        );
    }

    fn visit_block(&mut self, block: &'ast syn::Block) {
        self.push_scope();
        visit::visit_block(self, block);
        self.pop_scope();
    }

    fn visit_local(&mut self, local: &'ast syn::Local) {
        visit::visit_local(self, local);
        if matches!(local.pat, Pat::Wild(_)) {
            return;
        }
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
            self.reference_path(&path.path, Kind::Call, path.span(), false);
        } else {
            self.visit_expr(&call.func);
        }
        for arg in &call.args {
            self.visit_expr(arg);
        }
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        let (path, inferred) = match call.receiver.as_ref() {
            Expr::Path(path) if path.path.is_ident("self") => match &self.owner {
                Some(owner) => {
                    let mut full = Vec::with_capacity(owner.len() + 1);
                    full.extend(owner.iter().cloned());
                    full.push(identifier(&call.method));
                    (full, false)
                }
                None => (vec![identifier(&call.method)], true),
            },
            Expr::Path(path) => {
                let resolved = path.path.get_ident().and_then(|ident| {
                    let name = ident_str(ident, &mut self.scratch);
                    self.locals[self.locals_floor..]
                        .iter()
                        .rev()
                        .find(|(local, _)| local == name)
                        .and_then(|(_, ty)| ty.as_deref())
                });
                match resolved {
                    Some(owner) => {
                        let mut full = Vec::with_capacity(owner.len() + 1);
                        full.extend(owner.iter().cloned());
                        full.push(identifier(&call.method));
                        (full, false)
                    }
                    None => (vec![identifier(&call.method)], true),
                }
            }
            Expr::Struct(value) => {
                let mut full = Vec::with_capacity(value.path.segments.len() + 1);
                full.extend(value.path.segments.iter().map(|s| identifier(&s.ident)));
                full.push(identifier(&call.method));
                (full, false)
            }
            _ => (vec![identifier(&call.method)], true),
        };
        self.reference(path, Kind::Method, call.method.span(), inferred);
        visit::visit_expr_method_call(self, call);
    }

    fn visit_expr_path(&mut self, path: &'ast syn::ExprPath) {
        if path.qself.is_none() {
            self.reference_path(&path.path, Kind::Reference, path.span(), false);
        }
        visit::visit_expr_path(self, path);
    }

    fn visit_type_path(&mut self, path: &'ast syn::TypePath) {
        if path.qself.is_none() {
            self.reference_path(&path.path, Kind::Reference, path.span(), false);
        }
        visit::visit_type_path(self, path);
    }

    // Bounds in generics and where clauses, supertraits, `dyn Trait` and `impl Trait`.
    fn visit_trait_bound(&mut self, bound: &'ast syn::TraitBound) {
        self.reference_path(&bound.path, Kind::Reference, bound.path.span(), false);
        visit::visit_trait_bound(self, bound);
    }

    // Variant and struct patterns, such as `Status::Open(code)` or `Point { x, .. }`.
    // Plain path patterns already arrive through visit_expr_path.
    fn visit_pat_tuple_struct(&mut self, pattern: &'ast syn::PatTupleStruct) {
        if pattern.qself.is_none() {
            self.reference_path(&pattern.path, Kind::Reference, pattern.path.span(), false);
        }
        visit::visit_pat_tuple_struct(self, pattern);
    }

    fn visit_pat_struct(&mut self, pattern: &'ast syn::PatStruct) {
        if pattern.qself.is_none() {
            self.reference_path(&pattern.path, Kind::Reference, pattern.path.span(), false);
        }
        visit::visit_pat_struct(self, pattern);
    }

    fn visit_expr_struct(&mut self, value: &'ast syn::ExprStruct) {
        self.reference_path(&value.path, Kind::Reference, value.path.span(), false);
        visit::visit_expr_struct(self, value);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        let name = mac.path.segments.last().map_or("", |segment| {
            raw_ident_str(&segment.ident, &mut self.scratch)
        });
        let standard = matches!(
            name,
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
                | "ensure"
                | "bail"
                | "anyhow"
        );
        // `stringify!` turns its tokens into text, and a `macro_rules!` body is patterns.
        let other = self.every_macro && !matches!(name, "stringify" | "macro_rules");
        if name == "matches" {
            if let Ok((expr, pat, guard)) = mac.parse_body_with(
                |input: syn::parse::ParseStream<'_>| -> syn::Result<(Expr, Pat, Option<Expr>)> {
                    let expr: Expr = input.parse()?;
                    let _: Token![,] = input.parse()?;
                    let pat = Pat::parse_multi_with_leading_vert(input)?;
                    let guard = if input.peek(Token![if]) {
                        let _: Token![if] = input.parse()?;
                        Some(input.parse::<Expr>()?)
                    } else {
                        None
                    };
                    let _ = input.parse::<Option<Token![,]>>();
                    if !input.is_empty() {
                        return Err(input.error("unexpected trailing tokens in matches!"));
                    }
                    Ok((expr, pat, guard))
                },
            ) {
                self.visit_expr(&expr);
                self.push_scope();
                self.visit_pat(&pat);
                self.bind(&pat, None);
                if let Some(guard) = &guard {
                    self.visit_expr(guard);
                }
                self.pop_scope();
            }
            return;
        }
        if standard || other {
            if let Ok(expressions) =
                mac.parse_body_with(Punctuated::<Expr, Token![,]>::parse_terminated)
            {
                for expression in &expressions {
                    match expression {
                        // A field such as `info!(value = 3)` names no item.
                        Expr::Assign(field) if !standard => self.visit_expr(&field.right),
                        _ => self.visit_expr(expression),
                    }
                }
            } else if name == "vec"
                && let Ok((elem, count)) = mac.parse_body_with(
                    |input: syn::parse::ParseStream<'_>| -> syn::Result<(Expr, Expr)> {
                        let elem: Expr = input.parse()?;
                        let _: Token![;] = input.parse()?;
                        let count: Expr = input.parse()?;
                        if !input.is_empty() {
                            return Err(input.error("unexpected trailing tokens in vec!"));
                        }
                        Ok((elem, count))
                    },
                )
            {
                self.visit_expr(&elem);
                self.visit_expr(&count);
            }
        }
    }
}

fn raw_ident_str<'a>(ident: &syn::Ident, scratch: &'a mut String) -> &'a str {
    scratch.clear();
    let _ = write!(scratch, "{ident}");
    scratch.as_str()
}

fn ident_str<'a>(ident: &syn::Ident, scratch: &'a mut String) -> &'a str {
    let raw = raw_ident_str(ident, scratch);
    raw.strip_prefix("r#").unwrap_or(raw)
}

fn identifier(ident: &syn::Ident) -> String {
    let mut name = ident.to_string();
    if name.starts_with("r#") {
        name.drain(..2);
    }
    name
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
            segment.ident == "test" || segment.ident == "rstest" || segment.ident == "test_case"
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_macros_unions_trait_aliases_and_associated_items() {
        let src = concat!(
            "#[macro_export]\n",
            "macro_rules! check {\n",
            "    ($e:expr) => { assert!($e) };\n",
            "}\n\n",
            "some_item_macro!(ignored);\n\n",
            "pub union Payload {\n",
            "    pub int_val: i64,\n",
            "    pub float_val: f64,\n",
            "}\n\n",
            "pub trait SendSync = Send + Sync;\n\n",
            "pub trait Service {\n",
            "    type Output;\n",
            "    const VERSION: u32 = 1;\n",
            "    fn run(&self) -> Self::Output;\n",
            "}\n\n",
            "pub struct Worker;\n\n",
            "impl Service for Worker {\n",
            "    type Output = String;\n",
            "    const VERSION: u32 = 2;\n",
            "    fn run(&self) -> Self::Output {\n",
            "        String::new()\n",
            "    }\n",
            "}\n\n",
            "impl Worker {\n",
            "    pub const LIMIT: usize = 64;\n",
            "}\n",
        );
        let syntax = parse(src).unwrap();
        let found: Vec<_> = syntax
            .symbols
            .iter()
            .map(|s| (s.kind.as_str(), s.path.join("::")))
            .collect();
        assert_eq!(
            found,
            [
                ("macro", "check".into()),
                ("union", "Payload".into()),
                ("trait alias", "SendSync".into()),
                ("trait", "Service".into()),
                ("associated type", "Service::Output".into()),
                ("associated constant", "Service::VERSION".into()),
                ("method", "Service::run".into()),
                ("struct", "Worker".into()),
                ("associated type", "Worker::Output".into()),
                ("associated constant", "Worker::VERSION".into()),
                ("method", "Worker::run".into()),
                ("associated constant", "Worker::LIMIT".into()),
            ]
        );
        assert!(
            src[syntax.symbols[0].bytes.clone()].starts_with("#[macro_export]\nmacro_rules! check")
        );
        assert!(src[syntax.symbols[0].bytes.clone()].ends_with('}'));
        assert_eq!(
            &src[syntax.symbols[2].bytes.clone()],
            "pub trait SendSync = Send + Sync;"
        );
        assert_eq!(
            &src[syntax.symbols[11].bytes.clone()],
            "pub const LIMIT: usize = 64;"
        );
    }

    #[test]
    fn extracts_async_const_impl_trait_self_resolution_and_cfg_test_items() {
        let src = concat!(
            "use std::collections::{BTreeMap, HashMap as Map};\n\n",
            "macro_rules! make_const {\n",
            "    ($name:ident, $val:expr) => { pub const $name: usize = $val; };\n",
            "}\n\n",
            "pub trait Runner {\n",
            "    fn start(&self);\n",
            "}\n\n",
            "pub struct Job;\n\n",
            "impl Job {\n",
            "    pub const fn max_attempts() -> usize { 3 }\n",
            "    pub async fn run_once(&self) -> usize {\n",
            "        let r#type = Self::max_attempts();\n",
            "        let local_copy = r#type;\n",
            "        local_copy\n",
            "    }\n",
            "}\n\n",
            "impl Runner for Job {\n",
            "    fn start(&self) {\n",
            "        let _ = Self::max_attempts();\n",
            "    }\n",
            "}\n\n",
            "#[cfg(test)]\n",
            "mod tests {\n",
            "    fn helper() -> usize { 1 }\n",
            "    #[test]\n",
            "    fn sync_test() { assert_eq!(helper(), 1); }\n",
            "    #[tokio::test]\n",
            "    async fn async_test() { assert_eq!(super::Job::max_attempts(), 3); }\n",
            "    #[rstest]\n",
            "    fn param_test() {}\n",
            "}\n",
        );
        let syntax = parse(src).unwrap();
        let found: Vec<_> = syntax
            .symbols
            .iter()
            .map(|s| (s.kind.as_str(), s.path.join("::"), s.test))
            .collect();
        assert_eq!(
            found,
            [
                ("macro", "make_const".into(), false),
                ("trait", "Runner".into(), false),
                ("method", "Runner::start".into(), false),
                ("struct", "Job".into(), false),
                ("method", "Job::max_attempts".into(), false),
                ("method", "Job::run_once".into(), false),
                ("method", "Job::start".into(), false),
                ("module", "tests".into(), false),
                ("function", "tests::helper".into(), false),
                ("function", "tests::sync_test".into(), true),
                ("function", "tests::async_test".into(), true),
                ("function", "tests::param_test".into(), true),
            ]
        );
        // `impl Runner for Job` records a reference to `Runner`, and `Self::max_attempts()`
        // resolves to `Job::max_attempts` while local variables (`r#type`, `local_copy`) are skipped.
        assert!(
            syntax
                .references
                .iter()
                .any(|r| r.kind == Kind::Reference && r.path == ["Runner"])
        );
        assert_eq!(
            syntax
                .references
                .iter()
                .filter(|r| r.kind == Kind::Call && r.path == ["Job", "max_attempts"])
                .count(),
            2
        );
        assert!(
            !syntax
                .references
                .iter()
                .any(|r| r.path == ["type"] || r.path == ["local_copy"])
        );
        // Grouped `use` expands cleanly with alias preserved.
        assert_eq!(syntax.imports.len(), 2);
        assert_eq!(syntax.imports[0].path, ["std", "collections", "BTreeMap"]);
        assert_eq!(syntax.imports[1].alias.as_deref(), Some("Map"));
        assert_eq!(syntax.imports[1].path, ["std", "collections", "HashMap"]);
        // `#[cfg(test)] mod tests` byte span begins at the attribute for outline test-module folding.
        let tests_mod = &syntax.symbols[7];
        assert!(src[tests_mod.bytes.clone()].starts_with("#[cfg(test)]\nmod tests"));
    }

    #[test]
    fn nested_items_do_not_inherit_enclosing_function_locals() {
        let src = concat!(
            "fn helper() {}\n",
            "fn outer() {\n",
            "    let helper = 1;\n",
            "    fn inner() { helper(); }\n",
            "    let _ = helper;\n",
            "}\n",
        );
        let syntax = parse(src).unwrap();
        let inner_idx = syntax
            .symbols
            .iter()
            .position(|s| s.name == "inner")
            .unwrap();
        let calls: Vec<_> = syntax
            .references
            .iter()
            .filter(|r| r.path == ["helper"])
            .map(|r| (r.from, r.kind))
            .collect();
        assert_eq!(calls, [(Some(inner_idx), Kind::Call)]);
    }

    #[test]
    fn extracts_references_inside_ensure_bail_anyhow_vec_repeat_and_matches() {
        let src = concat!(
            "fn helper() -> i32 { 1 }\n",
            "fn guard_fn() -> bool { true }\n",
            "const COUNT: usize = 3;\n",
            "enum Status { Ok, Err }\n",
            "fn check(s: Status) -> Result<(), String> {\n",
            "    let _v = vec![helper(); COUNT];\n",
            "    let _m = matches!(s, Status::Ok if guard_fn());\n",
            "    let _shadowed = matches!(Some(1), Some(helper) if helper > 0);\n",
            "    ensure!(helper() > 0, \"bad\");\n",
            "    if false { bail!(\"{}\", helper()); }\n",
            "    let _e = anyhow!(\"{}\", helper());\n",
            "    Ok(())\n",
            "}\n",
        );
        let syntax = parse(src).unwrap();
        let helper_calls = syntax
            .references
            .iter()
            .filter(|r| r.kind == Kind::Call && r.path == ["helper"])
            .count();
        assert_eq!(
            helper_calls, 4,
            "expected helper() in vec repeat, ensure!, bail!, anyhow!: {:?}",
            syntax.references
        );
        assert!(
            syntax
                .references
                .iter()
                .any(|r| r.path == ["COUNT"] && r.kind == Kind::Reference),
            "expected COUNT in vec repeat: {:?}",
            syntax.references
        );
        assert!(
            syntax
                .references
                .iter()
                .any(|r| r.path == ["guard_fn"] && r.kind == Kind::Call),
            "expected guard_fn() in matches! guard: {:?}",
            syntax.references
        );
        assert!(
            !syntax
                .references
                .iter()
                .any(|r| r.path == ["helper"] && r.kind == Kind::Reference),
            "pattern-bound `helper` in matches! guard must stay shadowed: {:?}",
            syntax.references
        );
    }
}
