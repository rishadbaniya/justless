//! Integration suites for single features, built as one test binary so they link
//! once and their tests run in parallel. `cargo test --test testsuite mcp::` runs one.

mod codex;
mod common;
mod compaction;
mod compare;
mod delta;
mod global;
mod hook_fixes;
mod lookup;
mod mcp;
mod output_fixes;
mod read_batch;
mod redact;
mod report_fixes;
mod retirement;
