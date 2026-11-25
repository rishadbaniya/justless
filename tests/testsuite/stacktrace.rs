//! Workspace-aware stack-trace collapsing across Node/V8, Python, Java/Spring,
//! Go, and Rust, plus `[diagnostics]` upgrades (workspace frame surfacing,
//! archive pointers, timestamped dedup, head/tail duplicate suppression) and
//! persistent `known::PROGRESS` filtering on overflow.
#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

use crate::common::{BIN, executable, has, lacks, run_id, text};

struct Lab {
    _temp: TempDir,
    root: PathBuf,
    home: PathBuf,
    tools: PathBuf,
}

impl Lab {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let (root, home, tools) = (base.join("work"), base.join("home"), base.join("tools"));
        for dir in [&root, &home, &tools] {
            fs::create_dir_all(dir).unwrap();
        }
        Self {
            _temp: temp,
            root,
            home,
            tools,
        }
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn stub(&self, name: &str, body: &str) {
        executable(&self.tools.join(name), &format!("#!/bin/sh\n{body}"));
    }

    fn justless(&self, args: &[&str]) -> Output {
        let path = std::env::var("PATH").unwrap_or_default();
        Command::new(BIN)
            .args(args)
            .current_dir(&self.root)
            .env("PATH", format!("{}:{path}", self.tools.display()))
            .env("HOME", &self.home)
            .env("XDG_STATE_HOME", self.home.join("state"))
            .env("CLAUDE_CONFIG_DIR", self.home.join("claude"))
            .env("CODEX_HOME", self.home.join("codex"))
            .output()
            .unwrap()
    }

    fn replay_range(&self, id: &str, stream: &str, from: usize, to: usize) -> String {
        let out = self.justless(&[
            "log",
            id,
            "--stream",
            stream,
            "--from",
            &from.to_string(),
            "--to",
            &to.to_string(),
        ]);
        assert!(out.status.success(), "{}", text(&out.stderr));
        if stream == "stderr" {
            text(&out.stderr)
        } else {
            text(&out.stdout)
        }
    }

    fn replay_all(&self, id: &str, stream: &str) -> String {
        let out = self.justless(&["log", id, "--stream", stream]);
        assert!(out.status.success(), "{}", text(&out.stderr));
        if stream == "stderr" {
            text(&out.stderr)
        } else {
            text(&out.stdout)
        }
    }
}

/// Extract the first `(from, to)` range from `in {stream}:A-B]` in `shown`.
fn extract_collapsed_range(shown: &str, stream: &str) -> (usize, usize) {
    let needle = format!(" in {stream}:");
    let (_, after) = shown
        .split_once(&needle)
        .unwrap_or_else(|| panic!("missing {needle:?} in:\n{shown}"));
    let range = after.split(']').next().unwrap();
    let (a, b) = range.split_once('-').unwrap();
    (a.parse().unwrap(), b.parse().unwrap())
}

fn preview_without_footer(bytes: &[u8]) -> String {
    let stderr = text(bytes);
    stderr.split("\n[justless run-").next().unwrap().to_owned()
}

fn diagnostics_of(preview: &str) -> &str {
    preview
        .split_once("[diagnostics]\n")
        .map_or("", |(_, rest)| rest)
        .split("\n[output clipped.")
        .next()
        .unwrap()
}

#[test]
fn full_fit_node_express_pg_stack_trace_collapses_and_replays_exact_ranges() {
    let lab = Lab::new();
    let root = lab.root().display();
    let mut trace = String::from("Error: connect ECONNREFUSED 127.0.0.1:5432\n");
    trace.push_str("    at TCPConnectWrap.afterConnect (node:net:1495:16)\n");
    for i in 0..15 {
        trace.push_str(&format!(
            "    at Client._queryStep{i} ({root}/node_modules/pg/lib/client.js:{}:17)\n",
            100 + i
        ));
    }
    for i in 0..8 {
        trace.push_str(&format!(
            "    at processTicks{i} (node:internal/process/task_queues:{}:5)\n",
            80 + i
        ));
    }
    trace.push_str(&format!(
        "    at createOrder ({root}/src/orders.ts:42:11)\n"
    ));
    trace.push_str(&format!(
        "    at handleRequest ({root}/src/routes/orders.ts:18:5)\n"
    ));
    for i in 0..14 {
        trace.push_str(&format!(
            "    at Layer.handle{i} ({root}/node_modules/express/lib/router/layer.js:{}:5)\n",
            50 + i
        ));
    }
    fs::write(lab.root().join("err.txt"), &trace).unwrap();
    lab.stub("emit_err", "cat err.txt >&2\nexit 1\n");

    let out = lab.justless(&["run", "--hook", "--max-bytes", "8192", "--", "emit_err"]);
    assert_eq!(out.status.code(), Some(1));
    let id = run_id(&out);
    let shown = preview_without_footer(&out.stderr);
    let shown = shown.as_str();

    // >= 70% reduction on a 40-frame external trace while preserving 100% of signal.
    assert!(
        shown.len() * 10 <= trace.len() * 3,
        "expected >=70% reduction: {} vs {}\n{shown}",
        shown.len(),
        trace.len()
    );
    has(shown, "Error: connect ECONNREFUSED 127.0.0.1:5432");
    has(shown, "TCPConnectWrap.afterConnect (node:net:1495:16)");
    has(shown, "createOrder");
    has(shown, "handleRequest");
    has(
        shown,
        "[… 23 runtime/dependency frames (node_modules/pg, node:internal) in stderr:3-25]",
    );
    has(
        shown,
        "[… 14 runtime/dependency frames (node_modules/express) in stderr:28-41]",
    );

    // Exact archive range replay matches original lines 3..=25 byte-for-byte.
    let (from, to) = extract_collapsed_range(shown, "stderr");
    assert_eq!((from, to), (3, 25));
    let expected_slice: String = trace
        .split_inclusive('\n')
        .skip(from - 1)
        .take(to - from + 1)
        .collect();
    assert_eq!(lab.replay_range(&id, "stderr", from, to), expected_slice);
    assert_eq!(lab.replay_all(&id, "stderr"), trace);
}

#[test]
fn full_fit_python_java_go_and_rust_traces_achieve_high_reduction_with_exact_replay() {
    let lab = Lab::new();
    let root = lab.root().display();

    // 1. Python / FastAPI / SQLAlchemy traceback (bottom-most throw site + workspace frame).
    let mut py = String::from("Traceback (most recent call last):\n");
    for i in 0..12 {
        py.push_str(&format!(
            "  File \"{root}/.venv/lib/python3.11/site-packages/uvicorn/protocols/http/step{i}.py\", line {}, in run_asgi\n    await app(scope, receive, send)\n",
            100 + i
        ));
    }
    py.push_str(&format!(
        "  File \"{root}/src/orders.py\", line 42, in create_order\n    db.execute(stmt)\n"
    ));
    for i in 0..8 {
        py.push_str(&format!(
            "  File \"{root}/.venv/lib/python3.11/site-packages/sqlalchemy/engine/step{i}.py\", line {}, in _exec\n    return dialect.do_execute(cursor, sql)\n",
            200 + i
        ));
    }
    py.push_str(&format!(
        "  File \"{root}/.venv/lib/python3.11/site-packages/sqlalchemy/engine/base.py\", line 1967, in _exec_single_context\n    self.dialect.do_execute(cursor, str_statement)\n"
    ));
    py.push_str("sqlalchemy.exc.IntegrityError: UNIQUE constraint failed\n");

    // 2. Java / Spring stack trace with Unknown Source, Native Method, and Caused by.
    let mut java = String::from("java.lang.IllegalStateException: Order failed\n");
    java.push_str(
        "\tat org.springframework.dao.DataAccessUtils.requiredSingleResult(DataAccessUtils.java:100)\n",
    );
    for i in 0..14 {
        java.push_str(&format!(
            "\tat org.springframework.jdbc.core.JdbcTemplate.step{i}(JdbcTemplate.java:{})\n",
            200 + i
        ));
    }
    java.push_str(
        "\tat java.base/jdk.internal.reflect.NativeMethodAccessorImpl.invoke0(Native Method)\n",
    );
    java.push_str("\tat com.acme.orders.OrderController.create(OrderController.java:42)\n");
    for i in 0..10 {
        java.push_str(&format!(
            "\tat org.apache.catalina.core.FilterChain.step{i}(Unknown Source)\n"
        ));
    }
    java.push_str("Caused by: java.sql.SQLException: deadlock detected\n");
    java.push_str("\tat org.postgresql.core.v3.QueryExecutorImpl.receiveErrorResponse(QueryExecutorImpl.java:2725)\n");
    for i in 0..8 {
        java.push_str(&format!(
            "\tat org.postgresql.core.v3.QueryExecutorImpl.step{i}(QueryExecutorImpl.java:{})\n",
            300 + i
        ));
    }
    java.push_str("\tat com.acme.orders.OrderRepository.save(OrderRepository.java:88)\n");

    // 3. Go panic stack trace with workspace `internal/` path.
    let mut go =
        String::from("panic: runtime error: invalid memory address\n\ngoroutine 1 [running]:\n");
    go.push_str(
        "net/http.(*response).WriteHeader(0xc00010a000, 0x1f4)\n\t/usr/local/go/src/net/http/server.go:1160 +0x35\n",
    );
    for i in 0..10 {
        go.push_str(&format!(
            "github.com/gin-gonic/gin.(*Context).Step{i}(0xc00012a000)\n\t/go/pkg/mod/github.com/gin-gonic/gin@v1.9.1/context.go:{} +0x29\n",
            100 + i
        ));
    }
    go.push_str(&format!(
        "github.com/acme/app/internal/orders.(*Service).Create(0xc00012a000)\n\t{root}/internal/orders/service.go:42 +0x1a4\n"
    ));
    for i in 0..10 {
        go.push_str(&format!(
            "net/http.(*conn).serveStep{i}(0xc000148000, {{0x10a2000, 0xc000112000}})\n\t/usr/local/go/src/net/http/server.go:{} +0x5e8\n",
            2000 + i
        ));
    }

    // 4. Rust `RUST_BACKTRACE=1` stack trace.
    let mut rust = String::from(
        "thread 'worker' panicked at src/orders.rs:42:9:\ninvalid order total\nstack backtrace:\n",
    );
    rust.push_str("   0: std::panicking::begin_panic_handler\n             at /rustc/129f3b9964af4d4a709d1383930ade12dfe7c081/library/std/src/panicking.rs:652:5\n");
    for i in 1..=12 {
        rust.push_str(&format!(
            "  {i:>2}: tokio::runtime::task::raw::poll_step_{i}\n             at /home/u/.cargo/registry/src/index.crates.io-6f17d22bba15001f/tokio-1.38.0/src/runtime/task/raw.rs:{}:18\n",
            100 + i
        ));
    }
    rust.push_str("  13: my_app::orders::validate\n             at ./src/orders.rs:42:9\n");
    for i in 14..=24 {
        rust.push_str(&format!(
            "  {i:>2}: hyper::proto::h1::dispatch::poll_{i}\n             at /home/u/.cargo/registry/src/index.crates.io-6f17d22bba15001f/hyper-1.3.0/src/proto/h1/dispatch.rs:{}:12\n",
            200 + i
        ));
    }

    let cases: [(&str, &str, &[&str]); 4] = [
        (
            "python",
            &py,
            &[
                "Traceback (most recent call last):",
                "src/orders.py\", line 42, in create_order",
                "line 1967, in _exec_single_context",
                "sqlalchemy.exc.IntegrityError",
                "runtime/dependency frames (uvicorn)",
                "runtime/dependency frames (sqlalchemy)",
            ],
        ),
        (
            "java",
            &java,
            &[
                "java.lang.IllegalStateException: Order failed",
                "DataAccessUtils.requiredSingleResult",
                "com.acme.orders.OrderController.create(OrderController.java:42)",
                "Caused by: java.sql.SQLException: deadlock detected",
                "QueryExecutorImpl.receiveErrorResponse",
                "com.acme.orders.OrderRepository.save(OrderRepository.java:88)",
                "runtime/dependency frames (org.springframework",
                "runtime/dependency frames (org.apache",
                "runtime/dependency frames (org.postgresql",
            ],
        ),
        (
            "go",
            &go,
            &[
                "panic: runtime error: invalid memory address",
                "goroutine 1 [running]:",
                "net/http.(*response).WriteHeader",
                "internal/orders/service.go:42",
                "runtime/dependency frames (github.com/gin-gonic/gin)",
                "runtime/dependency frames (net/http)",
            ],
        ),
        (
            "rust",
            &rust,
            &[
                "thread 'worker' panicked at src/orders.rs:42:9:",
                "stack backtrace:",
                "0: std::panicking::begin_panic_handler",
                "13: my_app::orders::validate",
                "at ./src/orders.rs:42:9",
                "runtime/dependency frames (tokio)",
                "runtime/dependency frames (hyper)",
            ],
        ),
    ];

    for (name, trace, required) in cases {
        let file = format!("{name}.err");
        fs::write(lab.root().join(&file), trace).unwrap();
        lab.stub(&format!("run_{name}"), &format!("cat {file} >&2\nexit 1\n"));
        let out = lab.justless(&[
            "run",
            "--hook",
            "--fresh",
            "--max-bytes",
            "16384",
            "--",
            &format!("run_{name}"),
        ]);
        assert_eq!(out.status.code(), Some(1), "{name}");
        let id = run_id(&out);
        let shown = preview_without_footer(&out.stderr);
        let shown = shown.as_str();
        assert!(
            shown.len() * 10 <= trace.len() * 3,
            "{name}: expected >=70% reduction: {} vs {}\n{shown}",
            shown.len(),
            trace.len()
        );
        for needle in required {
            has(shown, needle);
        }
        let (from, to) = extract_collapsed_range(shown, "stderr");
        let expected_slice: String = trace
            .split_inclusive('\n')
            .skip(from - 1)
            .take(to - from + 1)
            .collect();
        assert_eq!(
            lab.replay_range(&id, "stderr", from, to),
            expected_slice,
            "{name}"
        );
        assert_eq!(lab.replay_all(&id, "stderr"), *trace, "{name}");
    }
}

#[test]
fn clipped_failure_surfaces_workspace_frame_behind_eleven_library_frames() {
    let lab = Lab::new();
    let root = lab.root().display();

    // Node trace sitting in the middle of a large clipped stderr stream, with
    // workspace frame at frame #12 (previously dropped by FOLLOW_LINES = 4!).
    let mut log = String::new();
    for i in 1..=120 {
        log.push_str(&format!("boot step {i:03} initialized subsystem ok\n"));
    }
    let trace_start = 121;
    log.push_str("Error: query timed out after 5000ms\n");
    log.push_str("    at Timeout._onTimeout (node:internal/timers:581:7)\n");
    for i in 1..=10 {
        log.push_str(&format!(
            "    at Pool._step{i} ({root}/node_modules/pg-pool/index.js:{}:12)\n",
            200 + i
        ));
    }
    log.push_str(&format!(
        "    at loadAccount ({root}/src/accounts.ts:77:9)\n"
    ));
    for i in 1..=8 {
        log.push_str(&format!(
            "    at Layer.handle{i} ({root}/node_modules/express/lib/router/layer.js:{}:5)\n",
            50 + i
        ));
    }
    let trace_end = trace_start + 1 + 1 + 10 + 1 + 8 - 1;
    for i in 1..=120 {
        log.push_str(&format!("shutdown step {i:03} drained worker ok\n"));
    }
    fs::write(lab.root().join("clipped_node.err"), &log).unwrap();
    lab.stub("clipped_node", "cat clipped_node.err >&2\nexit 1\n");

    let out = lab.justless(&[
        "run",
        "--fresh",
        "--max-bytes",
        "4096",
        "--",
        "clipped_node",
    ]);
    assert_eq!(out.status.code(), Some(1));
    let id = run_id(&out);
    let shown = preview_without_footer(&out.stderr);
    let shown = shown.as_str();
    let diag = diagnostics_of(shown);
    has(
        diag,
        &format!("Error: query timed out after 5000ms  [stderr:{trace_start}-{trace_end}]"),
    );
    has(diag, "Timeout._onTimeout (node:internal/timers:581:7)");
    has(diag, "loadAccount");
    has(diag, "src/accounts.ts:77:9");
    has(
        diag,
        &format!(
            "[… 10 runtime/dependency frames (node_modules/pg-pool) in stderr:{}-{}]",
            trace_start + 2,
            trace_start + 11
        ),
    );
    let (from, to) = extract_collapsed_range(diag, "stderr");
    let expected_slice: String = log
        .split_inclusive('\n')
        .skip(from - 1)
        .take(to - from + 1)
        .collect();
    assert_eq!(lab.replay_range(&id, "stderr", from, to), expected_slice);

    // Python traceback in a clipped stream where workspace frame sits BEFORE 7
    // library frames (14 lines, previously evicted by TRACEBACK_FRAMES = 10!).
    let mut py_log = String::new();
    for i in 1..=120 {
        py_log.push_str(&format!("worker batch {i:03} processed ok\n"));
    }
    py_log.push_str("Traceback (most recent call last):\n");
    for i in 1..=4 {
        py_log.push_str(&format!(
            "  File \"{root}/.venv/lib/python3.11/site-packages/django/core/handlers/step{i}.py\", line {}, in handle\n    response = wrapped_callback(request)\n",
            40 + i
        ));
    }
    py_log.push_str(&format!(
        "  File \"{root}/src/billing.py\", line 91, in charge_customer\n    gateway.submit(invoice)\n"
    ));
    for i in 1..=7 {
        py_log.push_str(&format!(
            "  File \"{root}/.venv/lib/python3.11/site-packages/httpx/_client{i}.py\", line {}, in send\n    return self._send_handling_auth(request)\n",
            100 + i
        ));
    }
    py_log.push_str("httpx.ReadTimeout: timed out reading response\n");
    for i in 1..=120 {
        py_log.push_str(&format!("worker cleanup {i:03} done\n"));
    }
    fs::write(lab.root().join("clipped_py.err"), &py_log).unwrap();
    lab.stub("clipped_py", "cat clipped_py.err >&2\nexit 1\n");

    let out = lab.justless(&["run", "--fresh", "--max-bytes", "4096", "--", "clipped_py"]);
    assert_eq!(out.status.code(), Some(1));
    let shown = preview_without_footer(&out.stderr);
    let shown = shown.as_str();
    let diag = diagnostics_of(shown);
    has(diag, "Traceback (most recent call last):  [stderr:121-");
    has(diag, "src/billing.py\", line 91, in charge_customer");
    has(diag, "httpx/_client7.py");
    has(diag, "httpx.ReadTimeout: timed out reading response");
}

#[test]
fn diagnostics_deduplicates_timestamped_errors_and_suppresses_head_tail_duplicates() {
    let lab = Lab::new();

    // Lines 1..=2: error wholly inside head (should NOT be duplicated in [diagnostics]).
    // Lines 3..=25: info filler so head stops before line 26.
    // Lines 26..=65: 40 timestamped copies of the same timeout error -> [×40, first stderr:26].
    // Line 66: distinct later error -> must NOT be crowded out, carries [stderr:66].
    // Lines 67..=260: info filler.
    // Line 261: error wholly inside tail (should NOT be duplicated in [diagnostics]).
    let mut log = String::new();
    log.push_str("ERROR head-only startup warning detail\n");
    for i in 2..=25 {
        log.push_str(&format!(
            "2026-11-25T14:00:{i:02}.000Z INFO startup subsystem {i} initialized ready\n"
        ));
    }
    for i in 0..40 {
        log.push_str(&format!(
            "2026-11-25T14:01:{i:02}.000Z ERROR upstream connection pool timed out\n"
        ));
    }
    log.push_str("2026-11-25T14:02:00.000Z ERROR ledger invariant violated for order #991\n");
    for i in 1..=200 {
        log.push_str(&format!(
            "2026-11-25T14:03:{:02}.000Z INFO background tick {i:03} completed normally\n",
            i % 60
        ));
    }
    log.push_str("ERROR tail-only final shutdown message\n");

    fs::write(lab.root().join("ts.err"), &log).unwrap();
    lab.stub("emit_ts", "cat ts.err >&2\nexit 1\n");

    let out = lab.justless(&["run", "--fresh", "--max-bytes", "4096", "--", "emit_ts"]);
    assert_eq!(out.status.code(), Some(1));
    let shown = preview_without_footer(&out.stderr);
    let shown = shown.as_str();
    let diag = diagnostics_of(shown);

    // Head-only and tail-only errors appear in head/tail, NOT duplicated in [diagnostics]!
    has(shown, "ERROR head-only startup warning detail");
    has(shown, "ERROR tail-only final shutdown message");
    lacks(diag, "ERROR head-only startup warning detail");
    lacks(diag, "ERROR tail-only final shutdown message");

    // 40 timestamped copies collapsed into 1 entry with exact first line pointer!
    has(
        diag,
        "2026-11-25T14:01:00.000Z ERROR upstream connection pool timed out  [×40, first stderr:26]",
    );
    assert_eq!(
        diag.matches("upstream connection pool timed out").count(),
        1,
        "{diag}"
    );
    // Distinct later error is surfaced with its exact archive line pointer!
    has(
        diag,
        "2026-11-25T14:02:00.000Z ERROR ledger invariant violated for order #991  [stderr:66]",
    );
}

#[test]
fn persistent_progress_filtering_keeps_clipped_docker_pip_and_xcodebuild_clean() {
    let lab = Lab::new();

    // 1. Clipped `docker build` overflowing max_bytes even after dropping progress lines.
    let mut docker_out = String::new();
    for i in 1..=120 {
        docker_out.push_str(&format!(
            "#{i} transferring context: 42.10MB 0.3s done\n#{i} CACHED\n"
        ));
    }
    for i in 1..=80 {
        docker_out.push_str(&format!(
            "#121 [builder 4/9] step output line {i:03} building artifact bundle payload\n"
        ));
    }
    docker_out.push_str("error: failed to solve: process \"/bin/sh -c npm run build\" did not complete successfully\n");
    docker_out.push_str("  detail: exit code 2 in stage builder\n");
    for i in 81..=180 {
        docker_out.push_str(&format!(
            "#122 [builder 5/9] trailing build step line {i:03} writing layer output\n"
        ));
    }
    for i in 123..=160 {
        docker_out.push_str(&format!("#{i} exporting layers 0.2s done\n"));
    }
    fs::write(lab.root().join("docker.log"), &docker_out).unwrap();
    lab.stub("docker", "cat docker.log\nexit 1\n");

    let out = lab.justless(&[
        "run",
        "--hook",
        "--fresh",
        "--max-bytes",
        "4096",
        "--",
        "docker",
        "build",
        ".",
    ]);
    assert_eq!(out.status.code(), Some(1));
    let id = run_id(&out);
    let shown = text(&out.stdout);
    assert!(shown.len() <= 4096, "len={}\n{shown}", shown.len());
    has(&shown, "docker build FAILED (exit 1)");
    lacks(&shown, "transferring context:");
    lacks(&shown, " CACHED\n");
    lacks(&shown, "exporting layers");
    has(
        &shown,
        "error: failed to solve: process \"/bin/sh -c npm run build\"",
    );
    has(&shown, "detail: exit code 2 in stage builder");
    assert_eq!(lab.replay_all(&id, "stdout"), docker_out);

    // 2. Clipped `pip install` overflowing max_bytes.
    let mut pip_out = String::new();
    for i in 1..=100 {
        pip_out.push_str(&format!(
            "Collecting pkg-{i}==1.2.{i}\nDownloading pkg_{i}-1.2.{i}-py3-none-any.whl (120 kB)\n"
        ));
    }
    for i in 1..=80 {
        pip_out.push_str(&format!(
            "  running build_ext step {i:03} compiling native extension target object\n"
        ));
    }
    pip_out.push_str("ERROR: Failed building wheel for native-ext\n");
    pip_out
        .push_str("  src/native.c:19:5: fatal error: openssl/ssl.h: No such file or directory\n");
    for i in 81..=160 {
        pip_out.push_str(&format!(
            "  cleanup native build directory entry {i:03} removing temp object\n"
        ));
    }
    for i in 101..=140 {
        pip_out.push_str(&format!(
            "Requirement already satisfied: dep-{i} in /venv/lib/python3.11/site-packages\n"
        ));
    }
    fs::write(lab.root().join("pip.log"), &pip_out).unwrap();
    lab.stub("pip", "cat pip.log\nexit 1\n");

    let out = lab.justless(&[
        "run",
        "--hook",
        "--fresh",
        "--max-bytes",
        "4096",
        "--",
        "pip",
        "install",
        "-r",
        "requirements.txt",
    ]);
    assert_eq!(out.status.code(), Some(1));
    let id = run_id(&out);
    let shown = text(&out.stdout);
    assert!(shown.len() <= 4096, "len={}\n{shown}", shown.len());
    has(&shown, "progress lines omitted]");
    lacks(&shown, "Collecting pkg-");
    lacks(&shown, "Downloading pkg_");
    lacks(&shown, "Requirement already satisfied:");
    has(&shown, "ERROR: Failed building wheel for native-ext");
    has(&shown, "openssl/ssl.h: No such file or directory");
    assert_eq!(lab.replay_all(&id, "stdout"), pip_out);

    // 3. Clipped `xcodebuild` overflowing max_bytes.
    let mut xc_out = String::new();
    for i in 1..=100 {
        xc_out.push_str(&format!(
            "CompileSwift normal arm64 /work/Sources/File{i}.swift (in target 'App')\n"
        ));
    }
    for i in 1..=80 {
        xc_out.push_str(&format!(
            "    /Applications/Xcode.app/Contents/Developer/Toolchains/swift-frontend step {i:03}\n"
        ));
    }
    xc_out.push_str(
        "Sources/Checkout.swift:42:12: error: cannot convert value of type 'String' to expected argument type 'Int'\n",
    );
    for i in 81..=160 {
        xc_out.push_str(&format!(
            "    note: build transcript detail line {i:03} archiving build artifacts\n"
        ));
    }
    for i in 1..=50 {
        xc_out.push_str(&format!(
            "CpResource /work/Build/App.app/res{i}.png /work/Resources/res{i}.png\n"
        ));
    }
    fs::write(lab.root().join("xcode.log"), &xc_out).unwrap();
    lab.stub("xcodebuild", "cat xcode.log\nexit 65\n");

    let out = lab.justless(&[
        "run",
        "--hook",
        "--fresh",
        "--max-bytes",
        "4096",
        "--",
        "xcodebuild",
        "-scheme",
        "App",
        "build",
    ]);
    assert_eq!(out.status.code(), Some(65));
    let id = run_id(&out);
    let shown = text(&out.stdout);
    assert!(shown.len() <= 4096, "len={}\n{shown}", shown.len());
    has(&shown, "progress lines omitted]");
    lacks(&shown, "CompileSwift normal arm64");
    lacks(&shown, "CpResource /work/Build");
    has(
        &shown,
        "Sources/Checkout.swift:42:12: error: cannot convert value of type 'String'",
    );
    assert_eq!(lab.replay_all(&id, "stdout"), xc_out);
}

#[test]
fn clipped_go_rust_and_java_traces_surface_deep_workspace_frames_and_preserve_redaction() {
    let lab = Lab::new();
    let root = lab.root().display();

    // Clipped Rust RUST_BACKTRACE=1 with workspace frame behind 11 std/tokio frames.
    let mut rust_log = String::new();
    for i in 1..=120 {
        rust_log.push_str(&format!("worker step {i:03} processing queue item ok\n"));
    }
    rust_log.push_str("thread 'worker' panicked at src/orders.rs:42:9:\n");
    rust_log.push_str("assertion failed: total > 0\n");
    rust_log.push_str("stack backtrace:\n");
    rust_log.push_str("   0: std::panicking::begin_panic_handler\n             at /rustc/129f3b9964/library/std/src/panicking.rs:652:5\n");
    for i in 1..=10 {
        rust_log.push_str(&format!(
            "  {i:>2}: tokio::runtime::task::raw::poll_{i}\n             at /home/u/.cargo/registry/src/index.crates.io-abc/tokio-1.38.0/src/runtime/task/raw.rs:{}:18\n",
            100 + i
        ));
    }
    rust_log.push_str("  11: my_app::orders::settle\n             at ./src/orders.rs:42:9\n");
    for i in 1..=120 {
        rust_log.push_str(&format!("worker shutdown {i:03} drained ok\n"));
    }
    fs::write(lab.root().join("clipped_rust.err"), &rust_log).unwrap();
    lab.stub("clipped_rust", "cat clipped_rust.err >&2\nexit 101\n");

    let out = lab.justless(&[
        "run",
        "--fresh",
        "--max-bytes",
        "4096",
        "--",
        "clipped_rust",
    ]);
    assert_eq!(out.status.code(), Some(101));
    let shown = preview_without_footer(&out.stderr);
    let diag = diagnostics_of(&shown);
    has(diag, "thread 'worker' panicked at src/orders.rs:42:9:");
    has(diag, "11: my_app::orders::settle");
    has(diag, "at ./src/orders.rs:42:9");
    has(diag, "runtime/dependency frames (tokio) in stderr:");

    // Clipped Go panic with workspace frame behind 11 stdlib/gin frames.
    let mut go_log = String::new();
    for i in 1..=120 {
        go_log.push_str(&format!("server request {i:03} served 200 OK\n"));
    }
    go_log.push_str("panic: nil pointer dereference\n\ngoroutine 1 [running]:\n");
    go_log.push_str("net/http.(*response).WriteHeader(0xc00010a000, 0x1f4)\n\t/usr/local/go/src/net/http/server.go:1160 +0x35\n");
    for i in 1..=10 {
        go_log.push_str(&format!(
            "github.com/gin-gonic/gin.(*Context).Step{i}(0xc00012a000)\n\t/go/pkg/mod/github.com/gin-gonic/gin@v1.9.1/context.go:{} +0x29\n",
            100 + i
        ));
    }
    go_log.push_str(&format!(
        "github.com/acme/app/internal/orders.(*Service).Create(0xc00012a000)\n\t{root}/internal/orders/service.go:42 +0x1a4\n"
    ));
    for i in 1..=120 {
        go_log.push_str(&format!("server cleanup {i:03} closed conn\n"));
    }
    fs::write(lab.root().join("clipped_go.err"), &go_log).unwrap();
    lab.stub("clipped_go", "cat clipped_go.err >&2\nexit 2\n");

    let out = lab.justless(&["run", "--fresh", "--max-bytes", "4096", "--", "clipped_go"]);
    assert_eq!(out.status.code(), Some(2));
    let shown = preview_without_footer(&out.stderr);
    let diag = diagnostics_of(&shown);
    has(diag, "panic: nil pointer dereference  [stderr:121-");
    has(diag, "internal/orders/service.go:42 +0x1a4");
    has(
        diag,
        "runtime/dependency frames (github.com/gin-gonic/gin) in stderr:",
    );

    // Clipped Java/Spring stack trace with workspace frame behind 11 Spring/JDK frames.
    let mut java_log = String::new();
    for i in 1..=120 {
        java_log.push_str(&format!("spring startup bean {i:03} initialized ok\n"));
    }
    java_log.push_str("java.lang.IllegalStateException: Order failed\n");
    java_log.push_str(
        "\tat org.springframework.dao.DataAccessUtils.requiredSingleResult(DataAccessUtils.java:100)\n",
    );
    for i in 1..=10 {
        java_log.push_str(&format!(
            "\tat org.springframework.jdbc.core.JdbcTemplate.step{i}(JdbcTemplate.java:{})\n",
            200 + i
        ));
    }
    java_log.push_str("\tat com.acme.orders.OrderController.create(OrderController.java:42)\n");
    for i in 1..=120 {
        java_log.push_str(&format!("spring shutdown bean {i:03} destroyed ok\n"));
    }
    fs::write(lab.root().join("clipped_java.err"), &java_log).unwrap();
    lab.stub("clipped_java", "cat clipped_java.err >&2\nexit 1\n");

    let out = lab.justless(&[
        "run",
        "--fresh",
        "--max-bytes",
        "4096",
        "--",
        "clipped_java",
    ]);
    assert_eq!(out.status.code(), Some(1));
    let shown = preview_without_footer(&out.stderr);
    let diag = diagnostics_of(&shown);
    has(
        diag,
        "java.lang.IllegalStateException: Order failed  [stderr:121-",
    );
    has(
        diag,
        "com.acme.orders.OrderController.create(OrderController.java:42)",
    );
    has(
        diag,
        "runtime/dependency frames (org.springframework) in stderr:",
    );

    // Redacted secret inside a stack trace is never leaked, split, or hidden.
    // Use a 36-char high-entropy GitHub token shape (split across literals so
    // secret scanners do not flag the test source).
    let secret = format!("{}{}9kLm3pQr7xVz2nWb8cYd4fGh6jKs1mNp5qRt", "gh", "p_");
    let mut red_trace = format!("Error: auth failed with token={secret}\n");
    red_trace.push_str("    at TCPConnectWrap.afterConnect (node:net:1495:16)\n");
    for i in 0..8 {
        red_trace.push_str(&format!(
            "    at Client.step{i} ({root}/node_modules/pg/lib/client.js:{}:10)\n",
            100 + i
        ));
    }
    red_trace.push_str(&format!(
        "    at Request.withToken (https://api.example.com/auth.js?token={secret}:55:9)\n"
    ));
    for i in 8..16 {
        red_trace.push_str(&format!(
            "    at Client.step{i} ({root}/node_modules/pg/lib/client.js:{}:10)\n",
            100 + i
        ));
    }
    red_trace.push_str(&format!("    at connectDb ({root}/src/db.ts:19:5)\n"));
    fs::write(lab.root().join("red.err"), &red_trace).unwrap();
    lab.stub("emit_red", "cat red.err >&2\nexit 1\n");

    let out = lab.justless(&[
        "run",
        "--hook",
        "--fresh",
        "--max-bytes",
        "8192",
        "--",
        "emit_red",
    ]);
    assert_eq!(out.status.code(), Some(1));
    let shown = text(&out.stderr);
    for window in secret.as_bytes().windows(8) {
        lacks(&shown, std::str::from_utf8(window).unwrap());
    }
    assert_eq!(
        shown.matches("[redacted:github-token:").count(),
        2,
        "both redacted markers must stay visible:\n{shown}"
    );
    has(&shown, "Request.withToken");
    has(&shown, "connectDb");
    has(
        &shown,
        "[… 8 runtime/dependency frames (node_modules/pg) in stderr:3-10]",
    );
    has(
        &shown,
        "[… 8 runtime/dependency frames (node_modules/pg) in stderr:12-19]",
    );
}
