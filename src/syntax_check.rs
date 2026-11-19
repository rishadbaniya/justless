//! Whether an edit leaves a file that its language's parser rejects. Rust files
//! are parsed whole with syn, as the index parses them, and the other languages
//! with their Tree-sitter grammar, whose ERROR and MISSING nodes mark the text
//! it could not fit.
//!
//! syn follows stable Rust, so any error it finds is certain. A bundled
//! Tree-sitter grammar can lag behind its language, such as TypeScript variance
//! annotations or Kotlin context parameters, and reports an ERROR node for valid
//! newer syntax. An error is certain there only when a token is missing or the
//! edit leaves brackets that no longer pair up, which no version of the language
//! accepts. Other errors are reported, and the edit is not blocked.

use std::ops::{ControlFlow, Range};
use std::time::{Duration, Instant};

use tree_sitter::{ParseOptions, ParseState, Parser, Tree};

use crate::polyglot::Grammar;

/// The most problems collected from one file.
const MAX_PROBLEMS: usize = 16;
/// How long one Tree-sitter parse may run before the check gives up on the file.
const PARSE_LIMIT: Duration = Duration::from_millis(400);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    pub bytes: Range<usize>,
    pub message: String,
    /// The grammar needed a token that is not there, such as a closing `)`.
    pub missing: bool,
    /// The message names an unclosed or unmatched bracket.
    pub bracket: bool,
}

/// What one parse of a text found.
struct Parse {
    problems: Vec<Problem>,
    /// The Tree-sitter tree, which Rust files do not have.
    tree: Option<Tree>,
}

impl Parse {
    fn clean() -> Self {
        Self {
            problems: Vec::new(),
            tree: None,
        }
    }

    /// Opening minus closing `(`, `[` and `{` among the grammar's punctuation
    /// tokens. Tokens such as `${`, `#{` and `\(` count their bracket, and text in
    /// strings and comments counts nothing.
    fn brackets(&self, text: &str) -> [isize; 3] {
        self.tree
            .as_ref()
            .map_or([0; 3], |tree| brackets(tree, text))
    }
}

/// One replaced span of an edit: its bytes before the edit and after it.
#[derive(Clone, Debug)]
pub struct Change {
    pub before: Range<usize>,
    pub after: Range<usize>,
}

impl Change {
    /// The one span that differs between `before` and `after`, between their
    /// common prefix and suffix.
    pub fn between(before: &str, after: &str) -> Self {
        let prefix = before
            .bytes()
            .zip(after.bytes())
            .take_while(|(a, b)| a == b)
            .count();
        let room = before.len().min(after.len()) - prefix;
        let suffix = before
            .bytes()
            .rev()
            .zip(after.bytes().rev())
            .take(room)
            .take_while(|(a, b)| a == b)
            .count();
        Self {
            before: prefix..before.len() - suffix,
            after: prefix..after.len() - suffix,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The file parses after the edit, and before it too when that was asked.
    Clean,
    /// The edit introduced this problem: the file parsed before the edit, or
    /// the problem starts in text the edit wrote and was not there before.
    Introduced {
        problem: Problem,
        parsed_before: bool,
        /// The file parsed before, and the problem is one that no version of the
        /// language accepts: any syn error, a missing token, or brackets the edit
        /// left unpaired. Only a certain problem blocks an edit.
        certain: bool,
        /// The edit changed how the file's brackets pair up.
        unbalanced: bool,
    },
    /// The file did not parse before the edit and does now.
    Fixed { was: Problem },
    /// The file did not parse before the edit and still fails where it did.
    Unchanged { problem: Problem },
    /// The file did not parse before the edit and now fails elsewhere, outside
    /// the text the edit wrote.
    Moved { was: Problem, now: Problem },
    /// A parse ran past `PARSE_LIMIT`, so the edit was not checked.
    GaveUp,
}

/// The parser that checks `file`, named for messages, or `None` when edits to
/// this kind of file are not checked. SQL is left out because its grammar
/// rejects most dialects: in local corpora of working code it found errors in
/// 93% of `.sql` files.
pub fn parser_name(file: &str) -> Option<&'static str> {
    if file.ends_with(".rs") {
        return Some("Rust");
    }
    Some(match Grammar::for_file(file)? {
        Grammar::Sql => return None,
        Grammar::C => "C",
        Grammar::Cpp => "C++",
        Grammar::Bash => "Bash",
        Grammar::JavaScript => "JavaScript",
        Grammar::TypeScript => "TypeScript",
        Grammar::Tsx => "TSX",
        Grammar::Python => "Python",
        Grammar::Go => "Go",
        Grammar::Java => "Java",
        Grammar::CSharp => "C#",
        Grammar::Ruby => "Ruby",
        Grammar::Php => "PHP",
        Grammar::Kotlin => "Kotlin",
        Grammar::Swift => "Swift",
        Grammar::Scala => "Scala",
        Grammar::Dart => "Dart",
        Grammar::Hcl => "HCL",
        Grammar::Lua => "Lua",
        Grammar::Zig => "Zig",
    })
}

/// Whether a certain problem in `file` blocks an edit. The C, C++ and Bash
/// grammars reject valid code, such as macros, Objective-C headers and the
/// POSIX `for NAME do` loop, so their problems are only reported: in local
/// corpora of working code they found errors in 43% of `.c`, 67% of `.h`, 45% of
/// `.cc` and 1.1% of `.sh` files, against at most 0.2% for the others.
fn blocks(file: &str) -> bool {
    !matches!(
        Grammar::for_file(file),
        Some(Grammar::C | Grammar::Cpp | Grammar::Bash)
    )
}

/// The parse problems in `text` read as `file`, in source order, or `None`
/// when no parser checks this kind of file or the parse gave up.
#[cfg(test)]
fn problems(file: &str, text: &str) -> Option<Vec<Problem>> {
    parse(file, text).map(|parse| parse.problems)
}

fn parse(file: &str, text: &str) -> Option<Parse> {
    parser_name(file)?;
    if file.ends_with(".rs") {
        let error = crate::rust_syntax::first_error(text);
        return Some(Parse {
            problems: error
                .map(|(bytes, message)| Problem {
                    bytes,
                    bracket: message.starts_with("unclosed") || message.starts_with("unmatched"),
                    message,
                    missing: false,
                })
                .into_iter()
                .collect(),
            tree: None,
        });
    }
    let grammar = Grammar::for_file(file)?;
    let mut parser = Parser::new();
    parser.set_language(&grammar.language()).ok()?;
    let started = Instant::now();
    let mut progress = |_: &ParseState| {
        if started.elapsed() > PARSE_LIMIT {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let bytes = text.as_bytes();
    let tree = parser.parse_with_options(
        &mut |offset, _| bytes.get(offset..).unwrap_or_default(),
        None,
        Some(ParseOptions::new().progress_callback(&mut progress)),
    )?;
    let mut found = Vec::new();
    let mut cursor = tree.walk();
    'walk: loop {
        let node = cursor.node();
        let descend = if node.is_missing() {
            found.push(Problem {
                bytes: node.start_byte()..node.start_byte(),
                message: format!("missing {}", describe(node.kind(), node.is_named())),
                missing: true,
                bracket: false,
            });
            false
        } else if node.is_error() {
            found.push(error_node(node));
            false
        } else {
            node.has_error()
        };
        if found.len() >= MAX_PROBLEMS {
            break;
        }
        if descend && cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                break 'walk;
            }
        }
    }
    drop(cursor);
    Some(Parse {
        problems: found,
        tree: Some(tree),
    })
}

/// Opening minus closing brackets among the punctuation tokens of `tree`, a
/// parse of `text`. See [`Parse::brackets`].
fn brackets(tree: &Tree, text: &str) -> [isize; 3] {
    let mut count = [0; 3];
    let mut add = |tokens: &str| {
        for c in tokens.chars() {
            match c {
                '(' => count[0] += 1,
                ')' => count[0] -= 1,
                '[' => count[1] += 1,
                ']' => count[1] -= 1,
                '{' => count[2] += 1,
                '}' => count[2] -= 1,
                _ => {}
            }
        }
    };
    let mut cursor = tree.walk();
    loop {
        let node = cursor.node();
        if node.is_error() {
            // Error recovery can skip text without making tokens of it, so the
            // bytes of an ERROR node that no child covers count by character.
            let mut at = node.start_byte();
            let mut children = node.walk();
            for child in node.children(&mut children) {
                add(text.get(at..child.start_byte()).unwrap_or_default());
                at = child.end_byte();
            }
            add(text.get(at..node.end_byte()).unwrap_or_default());
        } else if node.child_count() == 0 && !node.is_named() && !node.is_missing() {
            add(node.kind());
        }
        if cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return count;
            }
        }
    }
}

/// An ERROR node as a problem. A bracket among its tokens that is never closed,
/// or a closing one never opened, is the likeliest cause, so the problem points
/// at it. Otherwise it names the first token the grammar could not fit. Only
/// grammar names and keywords appear in the message, never the file's own text.
fn error_node(node: tree_sitter::Node) -> Problem {
    let mut open: Vec<tree_sitter::Node> = Vec::new();
    let mut unmatched = None;
    let mut first = None;
    let mut cursor = node.walk();
    let mut seen = 0;
    'walk: loop {
        let leaf = cursor.node();
        if leaf.child_count() == 0 && leaf.id() != node.id() {
            first.get_or_insert(leaf);
            match leaf.kind() {
                "(" | "[" | "{" => open.push(leaf),
                close @ (")" | "]" | "}") => {
                    let opener = match close {
                        ")" => "(",
                        "]" => "[",
                        _ => "{",
                    };
                    match open.iter().rposition(|node| node.kind() == opener) {
                        Some(index) => open.truncate(index),
                        None => {
                            unmatched.get_or_insert(leaf);
                        }
                    }
                }
                _ => {}
            }
            seen += 1;
            if seen > 4096 {
                break;
            }
        }
        if cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() || cursor.node().id() == node.id() {
                break 'walk;
            }
        }
    }
    let bracket = |leaf: tree_sitter::Node, what: &str| Problem {
        bytes: leaf.byte_range(),
        message: format!("{what} `{}`", leaf.kind()),
        missing: false,
        bracket: true,
    };
    if let Some(leaf) = open.first() {
        return bracket(*leaf, "unclosed");
    }
    if let Some(leaf) = unmatched {
        return bracket(leaf, "unmatched");
    }
    Problem {
        bytes: node.byte_range(),
        message: match first {
            Some(leaf) => format!("syntax error at {}", describe(leaf.kind(), leaf.is_named())),
            None => "syntax error".to_owned(),
        },
        missing: false,
        bracket: false,
    }
}

fn describe(kind: &str, named: bool) -> String {
    if named {
        kind.replace('_', " ")
    } else {
        format!("`{}`", kind.escape_debug())
    }
}

/// Where `position` in the file before the edit is after it, or `None` when
/// the edit replaced that byte. `changes` are in order and do not overlap.
fn mapped(position: usize, changes: &[Change]) -> Option<usize> {
    let mut shift = 0isize;
    for change in changes {
        if position < change.before.start {
            break;
        }
        if position < change.before.end {
            return None;
        }
        shift += change.after.len() as isize - change.before.len() as isize;
    }
    position.checked_add_signed(shift)
}

/// Judges an edit of `file` from `before`, or from no file, to `after`. Returns
/// `None` when the file's kind is not checked. With `need_before` false, a file
/// that parses after the edit is `Clean` without parsing the text before it.
/// With it true, both texts are parsed at once.
pub fn judge(
    file: &str,
    before: Option<&str>,
    after: &str,
    changes: &[Change],
    need_before: bool,
) -> Option<Verdict> {
    parser_name(file)?;
    let parsed = match before {
        Some(before) if need_before => both(file, before, after),
        _ => parse(file, after).and_then(|now| {
            if now.problems.is_empty() && !need_before {
                return Some((now, None));
            }
            let was = match before {
                Some(before) => parse(file, before)?,
                None => Parse::clean(),
            };
            Some((now, Some(was)))
        }),
    };
    let Some((now, was)) = parsed else {
        return Some(Verdict::GaveUp);
    };
    let Some(was) = was else {
        return Some(Verdict::Clean);
    };
    let Some(first_was) = was.problems.first() else {
        let Some(first) = now.problems.first() else {
            return Some(Verdict::Clean);
        };
        // A missing token, or a bracket claim that the counts confirm, says more
        // than the first error does.
        let unbalanced = now.brackets(after) != was.brackets(before.unwrap_or_default());
        let problem = now
            .problems
            .iter()
            .find(|problem| problem.missing || unbalanced && problem.bracket)
            .unwrap_or(first)
            .clone();
        let certain = blocks(file) && (file.ends_with(".rs") || problem.missing || unbalanced);
        return Some(Verdict::Introduced {
            problem,
            parsed_before: true,
            certain,
            unbalanced,
        });
    };
    let Some(first_now) = now.problems.first() else {
        return Some(Verdict::Fixed {
            was: first_was.clone(),
        });
    };
    let old = |problem: &Problem| {
        was.problems
            .iter()
            .any(|earlier| mapped(earlier.bytes.start, changes) == Some(problem.bytes.start))
    };
    let written = |problem: &Problem| {
        changes.iter().any(|change| {
            !change.after.is_empty()
                && change.after.start <= problem.bytes.start
                && problem.bytes.start <= change.after.end
        })
    };
    if let Some(problem) = now
        .problems
        .iter()
        .find(|problem| written(problem) && !old(problem))
    {
        // A file that did not parse before is never blocked.
        return Some(Verdict::Introduced {
            problem: problem.clone(),
            parsed_before: false,
            certain: false,
            unbalanced: false,
        });
    }
    Some(match now.problems.iter().find(|problem| !old(problem)) {
        None => Verdict::Unchanged {
            problem: first_now.clone(),
        },
        Some(problem) => Verdict::Moved {
            was: first_was.clone(),
            now: problem.clone(),
        },
    })
}

/// The parses after and before an edit, run on two threads so that the check
/// takes about as long as one parse, or `None` when either gave up. The second
/// thread gets the main thread's stack, which deeply nested source needs.
fn both(file: &str, before: &str, after: &str) -> Option<(Parse, Option<Parse>)> {
    std::thread::scope(|scope| {
        let worker = std::thread::Builder::new()
            .stack_size(crate::STACK_BYTES)
            .spawn_scoped(scope, || parse(file, before));
        let now = parse(file, after);
        let was = match worker {
            Ok(worker) => worker.join().ok().flatten(),
            Err(_) => parse(file, before),
        };
        Some((now?, Some(was?)))
    })
}

/// The 1-based line and column, in characters, of `offset` in `text`.
pub fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    let start = text[..offset].rfind('\n').map_or(0, |i| i + 1);
    (
        text[..start].matches('\n').count() + 1,
        text[start..offset].chars().count() + 1,
    )
}

/// `FILE:LINE:COLUMN: MESSAGE (PARSER parser)` for a problem in `text`.
pub fn located(file: &str, text: &str, problem: &Problem) -> String {
    let (line, column) = line_column(text, problem.bytes.start);
    let parser = parser_name(file).unwrap_or("language");
    format!(
        "{file}:{line}:{column}: {} ({parser} parser)",
        problem.message
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first problem as `LINE:COLUMN message`.
    fn first(file: &str, text: &str) -> Option<String> {
        let found = problems(file, text).expect("a checked language");
        found.first().map(|problem| {
            let (line, column) = line_column(text, problem.bytes.start);
            format!("{line}:{column} {}", problem.message)
        })
    }

    /// Judges replacing the first `old` in `before` with `new`.
    fn edit(file: &str, before: &str, old: &str, new: &str) -> (String, Option<Verdict>) {
        let start = before.find(old).unwrap();
        let after = format!("{}{new}{}", &before[..start], &before[start + old.len()..]);
        let change = Change {
            before: start..start + old.len(),
            after: start..start + new.len(),
        };
        let verdict = judge(file, Some(before), &after, &[change], true);
        (after, verdict)
    }

    #[test]
    fn problems_name_the_cause_and_where_it_starts() {
        for (file, text, expected) in [
            (
                "a.rs",
                "pub fn a() -> u32 {\n    1 +\n",
                "1:19 unclosed `{`",
            ),
            ("a.rs", "fn a() {}}\n", "1:10 unmatched `}`"),
            ("a.rs", "fn a() {}\n\"open", "2:1 unterminated string"),
            (
                "a.rs",
                "fn a() {}\n/* open",
                "2:1 unterminated block comment",
            ),
            (
                "a.rs",
                "fn a() {\n    let x = ;\n}\n",
                "2:13 expected an expression",
            ),
            (
                "a.rs",
                "fn a() -> u32 {\n    1 +\n}\n",
                "3:1 unexpected end of input, expected an expression",
            ),
            // A byte order mark, shebang and CRLF keep their places in the text.
            (
                "a.rs",
                "\u{feff}#!/usr/bin/env run\r\nfn a() {\r\n    let é = ;\r\n}\r\n",
                "3:13 expected an expression",
            ),
            ("a.py", "def f(:\n    return 1\n", "1:7 missing `)`"),
            ("a.py", "def f():\n    return (1\n", "2:12 unclosed `(`"),
            ("a.py", "def f():\n    return 1)\n", "2:13 unmatched `)`"),
            (
                "a.ts",
                "export function f(a: number {\n  return a;\n}\n",
                "1:28 missing `)`",
            ),
            ("a.go", "package main\nfunc f() {\n", "2:10 unclosed `{`"),
        ] {
            assert_eq!(
                first(file, text).as_deref(),
                Some(expected),
                "{file} {text:?}"
            );
        }
        for (file, text) in [
            ("a.rs", "\u{feff}#!/usr/bin/env run\nfn a() -> u32 { 1 }\n"),
            ("a.py", "def f():\n    return (1,\n            2)\n"),
            ("a.tsx", "export const A = () => <div>{1}</div>;\n"),
        ] {
            assert_eq!(first(file, text), None, "{file}");
        }
        // SQL, whose grammar rejects most dialects, and unknown kinds are not
        // checked. C, C++ and Bash are, though their problems never block.
        for file in ["a.sql", "a.txt", "Makefile"] {
            assert_eq!(problems(file, "int f( {"), None, "{file}");
        }
        for file in ["a.c", "a.h", "a.cpp", "a.sh"] {
            assert!(!problems(file, "int f( {").unwrap().is_empty(), "{file}");
        }
    }

    #[test]
    fn messages_quote_grammar_tokens_not_source_text() {
        let text = "def f():\n    secret_token_value = [1, 2\n";
        let found = problems("a.py", text).unwrap();
        assert!(!found.is_empty());
        for problem in found {
            assert!(!problem.message.contains("secret"), "{}", problem.message);
        }
    }

    #[test]
    fn an_edit_that_breaks_a_parsing_file_is_introduced() {
        let before = "pub fn a() -> u32 {\n    1\n}\n\npub fn b() -> u32 {\n    2\n}\n";
        let (_, verdict) = edit("lib.rs", before, "    1\n}\n", "    1 +\n");
        let Some(Verdict::Introduced {
            problem,
            parsed_before: true,
            certain: true,
            ..
        }) = verdict
        else {
            panic!("{verdict:?}");
        };
        assert_eq!(problem.message, "unclosed `{`");
        // A clean edit is clean, and an unchecked kind of file is not judged.
        assert_eq!(
            edit("lib.rs", before, "1\n", "10\n").1,
            Some(Verdict::Clean)
        );
        assert_eq!(edit("a.sql", "SELECT 1;\n", "1;", "(1;").1, None);
        assert_eq!(edit("a.txt", "a\n", "a", "(").1, None);
        // A new file counts as parsing before.
        assert!(matches!(
            judge("new.py", None, "def f(:\n", &[], false),
            Some(Verdict::Introduced {
                parsed_before: true,
                ..
            })
        ));
        assert_eq!(
            judge("new.py", None, "x = 1\n", &[], false),
            Some(Verdict::Clean)
        );
    }

    #[test]
    fn a_file_that_did_not_parse_reports_its_state() {
        let before = "def f(:\n    return 1\n\ndef g():\n    return 2\n";
        // Fixed.
        let (_, verdict) = edit("a.py", before, "def f(:", "def f():");
        assert!(
            matches!(verdict, Some(Verdict::Fixed { .. })),
            "{verdict:?}"
        );
        // An edit elsewhere leaves the error where it was.
        let (_, verdict) = edit("a.py", before, "return 2", "return 3");
        assert!(
            matches!(verdict, Some(Verdict::Unchanged { .. })),
            "{verdict:?}"
        );
        // So does an edit that changes the line count before it.
        let below = "x = 1\n\ndef g(:\n    return 2\n";
        let (_, verdict) = edit("a.py", below, "x = 1\n", "x = 1\ny = 2\nz = 3\n");
        assert!(
            matches!(verdict, Some(Verdict::Unchanged { .. })),
            "{verdict:?}"
        );
        // A new error in the written text is introduced, though the file did not parse.
        let (after, verdict) = edit("a.py", before, "return 2", "return (2");
        let Some(Verdict::Introduced {
            problem,
            parsed_before: false,
            certain: false,
            ..
        }) = verdict
        else {
            panic!("{verdict:?}");
        };
        assert_eq!(line_column(&after, problem.bytes.start), (5, 12));
        // Rust reports only the first error. One in the written text is new.
        let rust = "fn a() {\n    1\n}\nfn b() {\n    let = 2;\n}\n";
        let (_, verdict) = edit("a.rs", rust, "    1\n", "    let = 1;\n");
        assert!(
            matches!(
                verdict,
                Some(Verdict::Introduced {
                    parsed_before: false,
                    ..
                })
            ),
            "{verdict:?}"
        );
        let (_, verdict) = edit(
            "a.rs",
            rust,
            "let = 2;",
            "let x = 2;\n    let = 3;\n}\nfn c() {",
        );
        assert!(
            matches!(
                verdict,
                Some(Verdict::Introduced {
                    parsed_before: false,
                    ..
                })
            ),
            "{verdict:?}"
        );
        // Fixing the first error shows the next one, outside the written text: a
        // change of state, not a new error.
        let (_, verdict) = edit(
            "a.rs",
            "fn a() {}\nfn b() { let = 2; }\nfn c() { let = 3; }\n",
            "let = 2;",
            "let x = 2;",
        );
        assert!(
            matches!(verdict, Some(Verdict::Moved { .. })),
            "{verdict:?}"
        );
    }

    /// How an edit from `before` to `after` is judged: "clean", "certain" for a
    /// problem that blocks it, "reported" for one that does not.
    fn judged(file: &str, before: &str, after: &str) -> &'static str {
        let change = Change::between(before, after);
        match judge(file, Some(before), after, &[change], true) {
            Some(Verdict::Clean) => "clean",
            Some(Verdict::Introduced { certain: true, .. }) => "certain",
            Some(Verdict::Introduced { certain: false, .. }) => "reported",
            other => panic!("{file}: {other:?}"),
        }
    }

    #[test]
    fn only_missing_tokens_and_unpaired_brackets_block_tree_sitter_languages() {
        // Valid syntax newer than the bundled grammar, written into a file that
        // parsed: reported, never blocked, even where the grammar's error names a
        // bracket that the counts show is paired.
        for (file, before, after) in [
            (
                "a.ts",
                "interface Box<T> {\n  value: T;\n}\n",
                "interface Box<in out T> {\n  value: T;\n}\n",
            ),
            (
                "a.py",
                "print(m[0], end=\"\")\n",
                "print(m[*[0]], end=\"\")\n",
            ),
            (
                "a.py",
                "def f[T](x: T) -> T:\n    return x\n",
                "def f[T = int](x: T) -> T:\n    return x\n",
            ),
            (
                "a.kt",
                "fun f() {\n    log()\n}\n",
                "context(logger: Logger)\nfun f() {\n    logger.info(\"x\")\n}\n",
            ),
            (
                "a.kt",
                "fun f(x: Any) = when (x) {\n    is String -> 0\n    else -> 1\n}\n",
                "fun f(x: Any) = when (x) {\n    is String if x.isEmpty() -> 0\n    else -> 1\n}\n",
            ),
            (
                "a.kt",
                "val s = \"x\"\n",
                "val s = $$\"\"\"{\"name\": \"$$name\"}\"\"\"\n",
            ),
            (
                "A.java",
                "class A {}\n",
                "import module java.base;\nclass A {}\n",
            ),
            (
                "a.cs",
                "static class E { }\n",
                "static class E { extension(string s) { public bool Empty => s.Length == 0; } }\n",
            ),
            (
                "a.js",
                "class A {\n  x = 1;\n}\n",
                "class A {\n  accessor x = 1;\n}\n",
            ),
        ] {
            assert_eq!(judged(file, before, after), "reported", "{after}");
        }
        // Breakage that no version of the language accepts blocks the edit.
        for (file, before, after) in [
            (
                "a.py",
                "def f():\n    return (1)\n",
                "def f():\n    return (1\n",
            ),
            (
                "a.js",
                "function f() {\n  return 1;\n}\n",
                "function f() {\n  return 1;\n}\n}\n",
            ),
            (
                "a.ts",
                "export function f(a: number) {\n  if (a) {\n    return 1;\n  }\n}\n",
                "export function f(a: number) {\n  if (a) {\n    return 1;\n}\n",
            ),
            (
                "A.java",
                "class A { void f() { int x = 1; } }\n",
                "class A { void f() { int x = 1 } }\n",
            ),
            (
                "a.kt",
                "fun f() { println(\"x\") }\n",
                "fun f() { println(\"x\" }\n",
            ),
            (
                "a.swift",
                "func f() { print(\"x\") }\n",
                "func f() { print(\"x\" }\n",
            ),
            (
                "a.cs",
                "class A { void F() { if (true) { } } }\n",
                "class A { void F() { if (true) { } }\n",
            ),
            (
                "a.go",
                "package p\n\nfunc f() {\n}\n",
                "package p\n\nfunc f() {\n",
            ),
            ("a.rb", "def f\n  g(1)\nend\n", "def f\n  g(1\nend\n"),
            (
                "a.rs",
                "fn a() {\n    let x = 1;\n}\n",
                "fn a() {\n    let x = ;\n}\n",
            ),
        ] {
            assert_eq!(judged(file, before, after), "certain", "{after}");
        }
        // Other errors in a Tree-sitter language are reported without blocking, and
        // C, C++ and Bash never block.
        for (file, before, after) in [
            ("a.rb", "def f\n  1\nend\n", "def f\n  1\n"),
            (
                "a.py",
                "def f():\n    return 1\n",
                "def f()\n    return 1\n",
            ),
            (
                "a.php",
                "<?php\n$x = 1;\n$y = 2;\n",
                "<?php\n$x = 1\n$y = 2;\n",
            ),
            (
                "a.c",
                "int f(void) { return 0; }\n",
                "int f(void) { return (0; }\n",
            ),
            ("a.cpp", "int f() { return 0; }\n", "int f() { return 0; \n"),
            ("a.sh", "f() {\n  echo a\n}\n", "f() {\n  echo a\n"),
        ] {
            assert_eq!(judged(file, before, after), "reported", "{after}");
        }
    }

    #[test]
    fn positions_map_across_replacements_and_insertions() {
        let changes = [
            Change {
                before: 2..4,
                after: 2..7,
            },
            Change {
                before: 10..10,
                after: 13..15,
            },
        ];
        assert_eq!(mapped(1, &changes), Some(1));
        assert_eq!(mapped(2, &changes), None);
        assert_eq!(mapped(3, &changes), None);
        assert_eq!(mapped(4, &changes), Some(7));
        assert_eq!(mapped(9, &changes), Some(12));
        // Text at an insertion point moves past the inserted text.
        assert_eq!(mapped(10, &changes), Some(15));
        let change = Change::between("abcXYdef", "abcQdef");
        assert_eq!((change.before, change.after), (3..5, 3..4));
        let change = Change::between("aaa", "aaaa");
        assert_eq!((change.before, change.after), (3..3, 3..4));
        let change = Change::between("same", "same");
        assert_eq!((change.before, change.after), (4..4, 4..4));
        assert_eq!(line_column("é\nab", 4), (2, 2));
        assert_eq!(line_column("éa", 1), (1, 1));
    }

    /// Every fixture in `fixtures/parse_check` is valid code in a checked language,
    /// with macros, decorators, templates and recent syntax, so the check must find
    /// nothing in it.
    #[test]
    fn valid_code_in_every_checked_language_has_no_problems() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/parse_check");
        let mut failures = Vec::new();
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_str().unwrap().to_owned();
            let text = std::fs::read_to_string(&path).unwrap();
            let Some(parse) = parse(&name, &text) else {
                continue;
            };
            checked += 1;
            // Punctuation tokens such as `${` and `#[` pair up in valid code.
            if parse.brackets(&text) != [0; 3] {
                failures.push(format!("{name}: brackets {:?}", parse.brackets(&text)));
            }
            let found = parse.problems;
            for problem in found {
                let (line, column) = line_column(&text, problem.bytes.start);
                let source = text.lines().nth(line - 1).unwrap_or_default();
                failures.push(format!(
                    "{name}:{line}:{column}: {} | {source}",
                    problem.message
                ));
            }
        }
        assert!(checked >= 18, "{checked}");
        assert!(failures.is_empty(), "\n{}", failures.join("\n"));
    }
}
