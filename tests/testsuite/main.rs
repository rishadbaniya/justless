//! Integration suites for single features, built as one test binary so they link
//! once and their tests run in parallel. `cargo test --test testsuite mcp::` runs one.

mod cargo_reports;
mod codex;
mod codex_lists;
mod common;
mod compaction;
mod compare;
mod delta;
mod diff_compact;
mod discover;
mod git_views;
mod global;
mod grep_lists;
mod hook_fixes;
mod js_reports;
mod known_commands;
mod lookup;
mod mcp;
mod mixed_lists;
mod nested_reads;
mod output_fixes;
mod outside_reads;
mod piped;
mod read_batch;
mod read_fixes;
mod read_shapes;
mod redact;
mod report_fixes;
mod retirement;
mod source_upgrades;
