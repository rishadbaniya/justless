//! End-to-end integration tests exercising the compiled `justless` binary across
//! Wave-2 wired features:
//! - Live `logview` folding (`docker compose logs`, `kubectl logs --prefix`, `journalctl`)
//!   with secret redaction and exact `--unredacted` replay.
//! - Live extended DevOps inventories (`kubectl get events`, `kubectl get svc`,
//!   `helm list`, `docker images`) with warning/pending/failed/dangling retention,
//!   `[stdout:line]` pointers, and exact `justless log` replay.
//! - Live MCP session deduplication on `read` and `expand` (unchanged notices,
//!   post-edit `+`/`-` line deltas, and `fresh: true` bypass).
//! - Live `hook-agy` and `hook-codex` routing + execution for single-quoted commands,
//!   `timeout` wrappers, and merged `2>&1 | head` pipes, plus the full Antigravity
//!   `before-edit` -> `after-edit` -> `post-invocation` parse-error bridge lifecycle.
//! - Live OpenCode plugin duplicate early retirement and `session_id` forwarding.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};
use tempfile::TempDir;

use crate::common::{BIN, has, lacks, piped, run_id, text};

#[cfg(unix)]
use crate::common::executable;

/// Isolated workspace + fake `HOME` so no test touches host settings.
struct Sandbox {
    _temp: TempDir,
    base: PathBuf,
    root: PathBuf,
    bin: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let root = base.join("project");
        let bin = base.join("bin");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(base.join("home")).unwrap();
        fs::create_dir_all(&bin).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"wave2\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn ok() -> u32 {\n    42\n}\n").unwrap();
        Self {
            _temp: temp,
            base,
            root,
            bin,
        }
    }

    fn home(&self) -> PathBuf {
        self.base.join("home")
    }

    fn justless_dir(&self) -> PathBuf {
        PathBuf::from(BIN).parent().unwrap().to_path_buf()
    }

    fn path_env(&self) -> String {
        let sep = if cfg!(windows) { ";" } else { ":" };
        format!(
            "{}{sep}{}{sep}{}",
            self.justless_dir().display(),
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        )
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let home = self.home();
        let mut command = Command::new(BIN);
        command
            .args(args)
            .current_dir(&self.root)
            .env("PATH", self.path_env())
            .env("HOME", &home)
            .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
            .env("CODEX_HOME", home.join(".codex"))
            .env("XDG_STATE_HOME", home.join(".local/state"))
            .env_remove("CLAUDE_PROJECT_DIR")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .env_remove("JUSTLESS_SESSION")
            .env_remove("JUSTLESS_DISABLE");
        command
    }

    fn run_hook(&self, cmd_args: &[&str]) -> Output {
        let mut args = vec!["--root", self.root.to_str().unwrap(), "run", "--hook", "--"];
        args.extend_from_slice(cmd_args);
        self.cmd(&args).output().unwrap()
    }

    fn log(&self, id: &str, extra: &[&str]) -> Output {
        let mut args = vec!["--root", self.root.to_str().unwrap(), "log", id];
        args.extend_from_slice(extra);
        let out = self.cmd(&args).output().unwrap();
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        out
    }

    fn hook_json(&self, args: &[&str], input: &Value) -> Value {
        let mut cmd = self.cmd(args);
        let out = piped(&mut cmd, input.to_string().as_bytes());
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|err| panic!("{args:?}: {err}: {}", text(&out.stdout)))
    }

    #[cfg(unix)]
    fn write_fixture_stub(&self, name: &str, fixture_name: &str, body: &str) {
        let fixture_path = self.base.join(fixture_name);
        fs::write(&fixture_path, body).unwrap();
        let script = format!("#!/bin/sh\ncat '{}'\n", fixture_path.display());
        executable(&self.bin.join(name), &script);
    }
}

#[cfg(unix)]
fn assert_reduction_at_least(label: &str, raw_bytes: usize, shown_bytes: usize, min_pct: usize) {
    assert!(
        shown_bytes < raw_bytes,
        "{label}: expected reduction, raw={raw_bytes} shown={shown_bytes}"
    );
    let pct = (raw_bytes - shown_bytes) * 100 / raw_bytes;
    assert!(
        pct >= min_pct,
        "{label}: expected >={min_pct}% byte reduction, got {pct}% (raw={raw_bytes}, shown={shown_bytes})"
    );
}

// ---------------------------------------------------------------------------
// 1. Live `logview` through `justless run --hook`
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn live_logview_docker_compose_folds_redacts_and_replays_unredacted() {
    let sb = Sandbox::new();
    let secret = format!("{}{}Ab3Cd4Ef5Gh6Ij7Kl8Mn9Op0Qr1St2Uv3Wx4", "gh", "p_");

    let mut raw = String::new();
    let services = ["api-1", "worker-1", "postgres-1"];
    let mut error_line = 0usize;
    let mut line_no = 0usize;

    for i in 1..=210 {
        let svc = services[(i - 1) % services.len()];
        let sec = i % 60;
        let min = (i / 60) % 60;
        line_no += 1;
        match svc {
            "api-1" => raw.push_str(&format!(
                "api-1       | 2026-11-25T10:{min:02}:{sec:02}.120Z INFO GET /v1/orders/{i:04} status=200 latency={}ms req_id=req-{i:08x}\n",
                10 + (i % 25)
            )),
            "worker-1" => raw.push_str(&format!(
                "worker-1    | 2026-11-25T10:{min:02}:{sec:02}.240Z INFO processed billing job job_id=job-{i:08x} duration={}ms\n",
                20 + (i % 40)
            )),
            _ => raw.push_str(&format!(
                "postgres-1  | 2026-11-25T10:{min:02}:{sec:02}.360Z LOG checkpoint complete: wrote {} buffers (0.{}s)\n",
                100 + i,
                1 + (i % 9)
            )),
        }
        if i == 117 {
            line_no += 1;
            error_line = line_no;
            raw.push_str(
                "api-1       | 2026-11-25T10:01:57.901Z ERROR Failed to commit order tx_id=deadbeef1234: connection reset by peer\n",
            );
            line_no += 1;
            raw.push_str(&format!(
                "api-1       | 2026-11-25T10:01:58.004Z WARN auth retry rejected token={secret}\n"
            ));
        }
    }

    sb.write_fixture_stub("docker", "compose_logs.txt", &raw);

    let out = sb.run_hook(&["docker", "compose", "logs"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let preview = text(&out.stdout);
    let id = run_id(&out);

    assert_reduction_at_least("docker compose logs", raw.len(), preview.len(), 75);
    has(&preview, "Failed to commit order");
    has(&preview, &format!("[stdout:{error_line}]"));
    has(&preview, "[redacted:github-token:");
    lacks(&preview, &secret);

    // Default `justless log` stays redacted.
    let redacted_log = text(&sb.log(&id, &[]).stdout);
    has(&redacted_log, "[redacted:github-token:");
    lacks(&redacted_log, &secret);

    // Targeted line lookup via the error's archive pointer reproduces the exact error line.
    let from = error_line.to_string();
    let targeted = text(
        &sb.log(&id, &["--stream", "stdout", "--from", &from, "--to", &from])
            .stdout,
    );
    has(
        &targeted,
        "ERROR Failed to commit order tx_id=deadbeef1234: connection reset by peer",
    );

    // `--unredacted` replays the exact archived stdout byte-for-byte (including the raw secret).
    let unredacted = text(&sb.log(&id, &["--unredacted"]).stdout);
    assert_eq!(unredacted, raw);
}

#[cfg(unix)]
#[test]
fn live_logview_kubectl_prefix_and_journalctl_fold_and_replay_exact() {
    let sb = Sandbox::new();

    // 1. `kubectl logs --prefix` across 3 pods + 1 error line.
    let mut k8s_raw = String::new();
    let pods = [
        "payments-api-7f8b9c-aa11/app",
        "payments-api-7f8b9c-bb22/app",
        "payments-api-7f8b9c-cc33/app",
    ];
    let mut k8s_err_line = 0usize;
    let mut line_no = 0usize;
    for i in 1..=180 {
        let pod = pods[(i - 1) % pods.len()];
        let sec = i % 60;
        let min = (i / 60) % 60;
        line_no += 1;
        k8s_raw.push_str(&format!(
            "[pod/{pod}] 2026-11-25T10:{min:02}:{sec:02}.123Z INFO handled POST /v1/charges status=200 latency={}ms req_id=chg-{i:08x}\n",
            8 + (i % 19)
        ));
        if i == 95 {
            line_no += 1;
            k8s_err_line = line_no;
            k8s_raw.push_str(
                "[pod/payments-api-7f8b9c-bb22/app] 2026-11-25T10:01:35.999Z ERROR DatabaseTimeoutError: statement timeout exceeded on shard-4\n",
            );
        }
    }
    sb.write_fixture_stub("kubectl", "kubectl_logs.txt", &k8s_raw);

    let k8s_out = sb.run_hook(&["kubectl", "logs", "--prefix", "-l", "app=payments"]);
    assert!(k8s_out.status.success(), "{}", text(&k8s_out.stderr));
    let k8s_preview = text(&k8s_out.stdout);
    let k8s_id = run_id(&k8s_out);

    assert_reduction_at_least(
        "kubectl logs --prefix",
        k8s_raw.len(),
        k8s_preview.len(),
        70,
    );
    has(
        &k8s_preview,
        "DatabaseTimeoutError: statement timeout exceeded on shard-4",
    );
    has(&k8s_preview, &format!("[stdout:{k8s_err_line}]"));
    assert_eq!(text(&sb.log(&k8s_id, &[]).stdout), k8s_raw);

    // 2. `journalctl` syslog stream + 1 SIGSEGV error line.
    let mut j_raw = String::new();
    let mut j_err_line = 0usize;
    line_no = 0;
    for i in 1..=180 {
        let sec = i % 60;
        let min = (i / 60) % 60;
        line_no += 1;
        j_raw.push_str(&format!(
            "Nov 25 10:{min:02}:{sec:02} web-01 nginx[4210]: INFO upstream healthcheck ok target=10.0.4.{}:8080 latency={}ms\n",
            10 + (i % 20),
            2 + (i % 9)
        ));
        if i == 124 {
            line_no += 1;
            j_err_line = line_no;
            j_raw.push_str(
                "Nov 25 10:02:04 web-01 nginx[4219]: ERROR worker process 4219 exited on signal 11 (SIGSEGV)\n",
            );
        }
    }
    sb.write_fixture_stub("journalctl", "journalctl_logs.txt", &j_raw);

    let j_out = sb.run_hook(&["journalctl", "-u", "nginx.service", "-n", "200"]);
    assert!(j_out.status.success(), "{}", text(&j_out.stderr));
    let j_preview = text(&j_out.stdout);
    let j_id = run_id(&j_out);

    assert_reduction_at_least("journalctl", j_raw.len(), j_preview.len(), 70);
    has(
        &j_preview,
        "worker process 4219 exited on signal 11 (SIGSEGV)",
    );
    has(&j_preview, &format!("[stdout:{j_err_line}]"));
    assert_eq!(text(&sb.log(&j_id, &[]).stdout), j_raw);
}

// ---------------------------------------------------------------------------
// 2. Live extended DevOps inventories through `justless run --hook`
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn live_extended_devops_inventories_compact_retain_issues_and_replay_exact() {
    let sb = Sandbox::new();

    // --- 2a. `kubectl get events -A` ---
    let mut events_raw = String::from(
        "NAMESPACE   LAST SEEN   TYPE      REASON             OBJECT                         MESSAGE\n",
    );
    let mut warn_sched_line = 0usize;
    let mut warn_backoff_line = 0usize;
    let mut line_no = 1usize;
    for i in 1..=60 {
        let reason = match i % 4 {
            0 => "Scheduled",
            1 => "Pulled",
            2 => "Created",
            _ => "Started",
        };
        line_no += 1;
        events_raw.push_str(&format!(
            "prod        {i}m          Normal    {reason:<18} pod/web-deployment-{i:03}         Successfully completed lifecycle step {i}\n"
        ));
        if i == 22 {
            line_no += 1;
            warn_sched_line = line_no;
            events_raw.push_str(
                "prod        2m          Warning   FailedScheduling   pod/checkout-api-7c9d          0/8 nodes are available: 8 Insufficient memory\n",
            );
        }
        if i == 47 {
            line_no += 1;
            warn_backoff_line = line_no;
            events_raw.push_str(
                "payments    45s         Warning   BackOff            pod/worker-9f1a                Back-off restarting failed container worker\n",
            );
        }
    }

    // --- 2b. `kubectl get svc -A` ---
    let mut svc_raw = String::from(
        "NAMESPACE   NAME                  TYPE           CLUSTER-IP      EXTERNAL-IP      PORT(S)          AGE\n",
    );
    let mut pending_svc_line = 0usize;
    let mut external_svc_line = 0usize;
    line_no = 1;
    for i in 1..=50 {
        let ns = if i <= 25 { "default" } else { "platform" };
        line_no += 1;
        svc_raw.push_str(&format!(
            "{ns:<11} svc-internal-{i:02}       ClusterIP      10.96.10.{i:<6}  <none>           8080/TCP         12d\n"
        ));
        if i == 18 {
            line_no += 1;
            pending_svc_line = line_no;
            svc_raw.push_str(
                "payments    payments-gateway      LoadBalancer   10.96.20.5      <pending>        443:31443/TCP    9m\n",
            );
        }
        if i == 33 {
            line_no += 1;
            external_svc_line = line_no;
            svc_raw.push_str(
                "ingress     public-ingress        LoadBalancer   10.96.20.8      34.120.44.10     80:30080/TCP     45d\n",
            );
        }
    }

    // Dispatching `kubectl` stub supporting `get events`, `get svc`, and `-o json`.
    let events_file = sb.base.join("events.txt");
    let svc_file = sb.base.join("svc.txt");
    fs::write(&events_file, &events_raw).unwrap();
    fs::write(&svc_file, &svc_raw).unwrap();
    let kubectl_script = format!(
        "#!/bin/sh\n\
         for a in \"$@\"; do\n\
           if [ \"$a\" = \"json\" ] || [ \"$a\" = \"-ojson\" ]; then\n\
             printf '{{\"items\":[{{\"kind\":\"Event\",\"reason\":\"Scheduled\"}}]}}\\n'\n\
             exit 0\n\
           fi\n\
         done\n\
         case \"$1 $2\" in\n\
           'get events') cat '{}' ;;\n\
           'get svc') cat '{}' ;;\n\
           *) echo \"unexpected kubectl args: $*\" >&2; exit 2 ;;\n\
         esac\n",
        events_file.display(),
        svc_file.display(),
    );
    executable(&sb.bin.join("kubectl"), &kubectl_script);

    // Run `kubectl get events -A`
    let ev_out = sb.run_hook(&["kubectl", "get", "events", "-A"]);
    assert!(ev_out.status.success(), "{}", text(&ev_out.stderr));
    let ev_preview = text(&ev_out.stdout);
    let ev_id = run_id(&ev_out);
    assert_reduction_at_least("kubectl get events", events_raw.len(), ev_preview.len(), 65);
    has(&ev_preview, "FailedScheduling");
    has(&ev_preview, "pod/checkout-api-7c9d");
    has(&ev_preview, &format!("[stdout:{warn_sched_line}]"));
    has(&ev_preview, "BackOff");
    has(&ev_preview, "pod/worker-9f1a");
    has(&ev_preview, &format!("[stdout:{warn_backoff_line}]"));
    assert_eq!(text(&sb.log(&ev_id, &[]).stdout), events_raw);

    // Run `kubectl get svc -A`
    let svc_out = sb.run_hook(&["kubectl", "get", "svc", "-A"]);
    assert!(svc_out.status.success(), "{}", text(&svc_out.stderr));
    let svc_preview = text(&svc_out.stdout);
    let svc_id = run_id(&svc_out);
    assert_reduction_at_least("kubectl get svc", svc_raw.len(), svc_preview.len(), 65);
    has(&svc_preview, "attention: payments/payments-gateway");
    has(&svc_preview, "<pending>");
    has(&svc_preview, &format!("[stdout:{pending_svc_line}]"));
    has(&svc_preview, "external: ingress/public-ingress");
    has(&svc_preview, "34.120.44.10");
    has(&svc_preview, &format!("[stdout:{external_svc_line}]"));
    has(&svc_preview, "Internal services:");
    assert_eq!(text(&sb.log(&svc_id, &[]).stdout), svc_raw);

    // Machine-format `-o json` is never reformatted as a table inventory.
    let json_out = sb.run_hook(&["kubectl", "get", "events", "-o", "json"]);
    assert!(json_out.status.success(), "{}", text(&json_out.stderr));
    let json_preview = text(&json_out.stdout);
    lacks(&json_preview, "kubectl get events:");
    has(&json_preview, "Scheduled");

    // --- 2c. `helm list -A` ---
    let mut helm_raw =
        String::from("NAME\tNAMESPACE\tREVISION\tUPDATED\tSTATUS\tCHART\tAPP VERSION\n");
    let mut failed_helm_line = 0usize;
    let mut pending_helm_line = 0usize;
    line_no = 1;
    for i in 1..=48 {
        let ns = if i <= 24 { "prod" } else { "observability" };
        let chart = if i % 2 == 0 {
            "service-base-1.4.2"
        } else {
            "metrics-agent-2.1.0"
        };
        line_no += 1;
        helm_raw.push_str(&format!(
            "release-{i:02}\t{ns}\t3\t2026-11-20 12:00:00.000000 +0000 UTC\tdeployed\t{chart}\t1.0.{i}\n"
        ));
        if i == 15 {
            line_no += 1;
            failed_helm_line = line_no;
            helm_raw.push_str(
                "billing-api\tprod\t14\t2026-11-25 09:50:00.000000 +0000 UTC\tfailed\tbilling-2.4.0\t2.4.0\n",
            );
        }
        if i == 31 {
            line_no += 1;
            pending_helm_line = line_no;
            helm_raw.push_str(
                "search-indexer\tprod\t7\t2026-11-25 09:58:00.000000 +0000 UTC\tpending-upgrade\tsearch-1.9.1\t1.9.1\n",
            );
        }
    }
    sb.write_fixture_stub("helm", "helm_list.txt", &helm_raw);

    let helm_out = sb.run_hook(&["helm", "list", "-A"]);
    assert!(helm_out.status.success(), "{}", text(&helm_out.stderr));
    let helm_preview = text(&helm_out.stdout);
    let helm_id = run_id(&helm_out);
    assert_reduction_at_least("helm list", helm_raw.len(), helm_preview.len(), 65);
    has(&helm_preview, "billing-api");
    has(&helm_preview, "failed");
    has(&helm_preview, &format!("[stdout:{failed_helm_line}]"));
    has(&helm_preview, "search-indexer");
    has(&helm_preview, "pending-upgrade");
    has(&helm_preview, &format!("[stdout:{pending_helm_line}]"));
    assert_eq!(text(&sb.log(&helm_id, &[]).stdout), helm_raw);

    // --- 2d. `docker images` ---
    let mut images_raw = String::from(
        "REPOSITORY                     TAG          IMAGE ID       CREATED        SIZE\n",
    );
    line_no = 1;
    for i in 1..=14 {
        line_no += 1;
        images_raw.push_str(&format!(
            "ghcr.io/acme/api               v1.{i:<8}  a1b2c3d4{i:04x}   {i} days ago    245MB\n"
        ));
    }
    for i in 1..=12 {
        line_no += 1;
        images_raw.push_str(&format!(
            "ghcr.io/acme/worker            v2.{i:<8}  b2c3d4e5{i:04x}   {i} days ago    180MB\n"
        ));
    }
    for i in 1..=10 {
        line_no += 1;
        images_raw.push_str(&format!(
            "postgres                       16.{i:<8}  c3d4e5f6{i:04x}   {i} weeks ago   412MB\n"
        ));
    }
    let dangling_first = line_no + 1;
    for i in 1..=4 {
        line_no += 1;
        images_raw.push_str(&format!(
            "<none>                         <none>       d4e5f600{i:04x}   {i} days ago    310MB\n"
        ));
    }
    let dangling_last = line_no;
    sb.write_fixture_stub("docker", "docker_images.txt", &images_raw);

    let img_out = sb.run_hook(&["docker", "images"]);
    assert!(img_out.status.success(), "{}", text(&img_out.stderr));
    let img_preview = text(&img_out.stdout);
    let img_id = run_id(&img_out);
    assert_reduction_at_least("docker images", images_raw.len(), img_preview.len(), 60);
    has(&img_preview, "dangling");
    has(
        &img_preview,
        &format!("[stdout:{dangling_first}-{dangling_last}]"),
    );
    has(&img_preview, "ghcr.io/acme/api (14 tags:");
    has(&img_preview, "ghcr.io/acme/worker (12 tags:");
    has(&img_preview, "postgres (10 tags:");
    assert_eq!(text(&sb.log(&img_id, &[]).stdout), images_raw);
}

// ---------------------------------------------------------------------------
// 3. Live MCP session dedup on `read` and `expand`
// ---------------------------------------------------------------------------

struct LiveMcp {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
}

impl LiveMcp {
    fn start(root: &Path) -> Self {
        let mut child = Command::new(BIN)
            .args(["mcp", "--root"])
            .arg(root)
            .current_dir(root)
            .env_remove("CLAUDE_PROJECT_DIR")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
        }
    }

    fn send(&mut self, msg: Value) -> Value {
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
        let mut line = String::new();
        let read = self.stdout.read_line(&mut line).unwrap();
        assert!(read > 0, "MCP server closed stdout unexpectedly");
        let parsed: Value = serde_json::from_str(&line)
            .unwrap_or_else(|err| panic!("invalid JSON-RPC line {line:?}: {err}"));
        assert_eq!(parsed["jsonrpc"], "2.0", "{parsed}");
        parsed
    }

    fn call(&mut self, id: &str, name: &str, arguments: Value) -> String {
        let resp = self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        }));
        assert_eq!(resp["id"], id, "{resp}");
        assert_eq!(resp["result"]["isError"], false, "{resp:#}");
        resp["result"]["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["text"].as_str().unwrap())
            .collect()
    }

    fn finish(self) {
        drop(self.stdin);
        drop(self.stdout);
        let output = self.child.wait_with_output().unwrap();
        assert!(output.status.success(), "{}", text(&output.stderr));
    }
}

#[test]
fn live_mcp_session_dedups_read_and_expand_with_deltas_and_fresh() {
    let sb = Sandbox::new();
    let mut big = String::new();
    for i in 1..=45 {
        big.push_str(&format!(
            "pub fn compute_{i:02}(input: u64) -> u64 {{\n    input.wrapping_add({i})\n}}\n\n"
        ));
    }
    let big_path = sb.root.join("src/big.rs");
    fs::write(&big_path, &big).unwrap();

    let expected_l1_80: String = big.split_inclusive('\n').take(80).collect();

    let mut mcp = LiveMcp::start(&sb.root);
    let init = mcp.send(json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "wave2-test", "version": "1"}
        }
    }));
    assert_eq!(init["result"]["serverInfo"]["name"], "justless");

    // 1. `read` twice on a large file: 1st returns full outline, 2nd returns 1-line notice,
    //    `fresh: true` returns full outline verbatim.
    let read1 = mcp.call("r1", "read", json!({"file": "src/big.rs", "outline": true}));
    has(&read1, "revision ");
    has(&read1, "compute_01");
    has(&read1, "compute_10");

    let read2 = mcp.call("r2", "read", json!({"file": "src/big.rs", "outline": true}));
    assert_eq!(read2.lines().count(), 1, "{read2}");
    has(&read2, "src/big.rs · outline unchanged at ");
    assert!(read2.len() < read1.len(), "read2={read2:?}");

    let read_fresh = mcp.call(
        "r3",
        "read",
        json!({"file": "src/big.rs", "outline": true, "fresh": true}),
    );
    assert_eq!(read_fresh, read1);

    // 2. `expand` twice on `L1-80`: 1st returns exact raw span, 2nd returns 1-line notice.
    let exp1 = mcp.call(
        "e1",
        "expand",
        json!({"file": "src/big.rs", "selector": "L1-80"}),
    );
    assert_eq!(exp1, expected_l1_80);

    let exp2 = mcp.call(
        "e2",
        "expand",
        json!({"file": "src/big.rs", "selector": "L1-80"}),
    );
    assert_eq!(exp2.lines().count(), 1, "{exp2}");
    has(&exp2, "src/big.rs:1-80 · unchanged since you read it at ");
    assert!(exp2.len() < exp1.len(), "exp2={exp2:?}");

    // 3. Edit 1 line inside L1-80 mid-session: 3rd `expand` emits exact compact `+`/`-` delta.
    let edited = big.replace("wrapping_add(10)", "wrapping_add(9000)");
    fs::write(&big_path, &edited).unwrap();
    let expected_edited_l1_80: String = edited.split_inclusive('\n').take(80).collect();

    let exp3 = mcp.call(
        "e3",
        "expand",
        json!({"file": "src/big.rs", "selector": "L1-80"}),
    );
    has(&exp3, "src/big.rs:1-80 · 79 of 80 lines unchanged");
    has(&exp3, "+ 38 |     input.wrapping_add(9000)\n");
    has(&exp3, "-    |     input.wrapping_add(10)\n");
    lacks(&exp3, "compute_02");
    assert!(exp3.len() < expected_edited_l1_80.len(), "{exp3}");

    // 4. `fresh: true` returns the exact full updated span verbatim.
    let exp_fresh = mcp.call(
        "e4",
        "expand",
        json!({"file": "src/big.rs", "selector": "L1-80", "fresh": true}),
    );
    assert_eq!(exp_fresh, expected_edited_l1_80);

    mcp.finish();
}

// ---------------------------------------------------------------------------
// 4. Live hook routing + execution of Wave-2 hook upgrades
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn live_hook_routing_and_execution_for_agy_and_codex_wave2_upgrades() {
    let sb = Sandbox::new();

    // Stub `rg` verifying the single-quoted regex `\bfoo\b` reaches argv verbatim.
    executable(
        &sb.bin.join("rg"),
        "#!/bin/sh\n\
         if [ \"$1\" != '\\bfoo\\b' ] || [ \"$2\" != 'src' ]; then\n\
           echo \"bad rg argv: [$1] [$2]\" >&2\n\
           exit 9\n\
         fi\n\
         i=1\n\
         while [ \"$i\" -le 140 ]; do\n\
           echo \"src/lib.rs:$i:let foo = $i; // padding padding padding padding padding\"\n\
           i=$((i + 1))\n\
         done\n",
    );

    // Stub `cargo` writing both stdout and stderr lines and honoring STUB_CARGO_EXIT.
    executable(
        &sb.bin.join("cargo"),
        "#!/bin/sh\n\
         i=1\n\
         while [ \"$i\" -le 80 ]; do\n\
           echo \"cargo-out-$i padding padding padding padding padding padding\"\n\
           echo \"cargo-err-$i padding padding padding padding padding padding\" >&2\n\
           i=$((i + 1))\n\
         done\n\
         exit \"${STUB_CARGO_EXIT:-0}\"\n",
    );

    let route = |client: &str, command: &str| -> (String, Value) {
        let input = match client {
            "hook-agy" => json!({
                "cwd": sb.root,
                "workspacePaths": [sb.root],
                "conversationId": "conv-wave2-exec",
                "toolCall": {"name": "run_command", "args": {"CommandLine": command}}
            }),
            "hook-codex" => json!({
                "hook_event_name": "PreToolUse",
                "tool_name": "Bash",
                "session_id": "codex-wave2-exec",
                "cwd": sb.root,
                "tool_input": {"command": command}
            }),
            _ => unreachable!(),
        };
        let out = sb.hook_json(&[client, "before-command"], &input);
        let rewritten = match client {
            "hook-agy" => {
                assert_eq!(out["decision"], "allow", "{client} {command}: {out}");
                out["overwrite"]["CommandLine"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{client} did not rewrite {command}: {out}"))
                    .to_owned()
            }
            "hook-codex" => out["hookSpecificOutput"]["updatedInput"]["command"]
                .as_str()
                .unwrap_or_else(|| panic!("{client} did not rewrite {command}: {out}"))
                .to_owned(),
            _ => unreachable!(),
        };
        (rewritten, out)
    };

    let run_sh = |script: &str, exit_env: Option<&str>| -> Output {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script])
            .current_dir(&sb.root)
            .env("PATH", sb.path_env())
            .env("HOME", sb.home())
            .env("XDG_STATE_HOME", sb.home().join(".local/state"))
            .env_remove("JUSTLESS_DISABLE");
        if let Some(code) = exit_env {
            cmd.env("STUB_CARGO_EXIT", code);
        }
        cmd.output().unwrap()
    };

    let has_timeout = Command::new("timeout").arg("--version").output().is_ok();

    for client in ["hook-agy", "hook-codex"] {
        // a) Single-quoted `rg '\bfoo\b' src`
        let (rg_cmd, _) = route(client, r"rg '\bfoo\b' src");
        has(&rg_cmd, "justless run --hook ");
        has(&rg_cmd, r"-- rg '\bfoo\b' src");
        let rg_res = run_sh(&rg_cmd, None);
        assert!(
            rg_res.status.success(),
            "{client} rg failed: stderr={}",
            text(&rg_res.stderr)
        );
        has(&text(&rg_res.stdout), "src/lib.rs");
        has(&text(&rg_res.stderr), "[justless run-");

        // b) `timeout 60 cargo test` (both exit 0 and exit 101 preserved!)
        let (timeout_cmd, _) = route(client, "timeout 60 cargo test");
        has(&timeout_cmd, "justless run --hook ");
        has(&timeout_cmd, "-- timeout 60 cargo test");
        if has_timeout {
            let ok_res = run_sh(&timeout_cmd, Some("0"));
            assert_eq!(
                ok_res.status.code(),
                Some(0),
                "{client} timeout ok: stderr={}",
                text(&ok_res.stderr)
            );
            has(&text(&ok_res.stderr), "[justless run-");

            let fail_res = run_sh(&timeout_cmd, Some("101"));
            assert_eq!(
                fail_res.status.code(),
                Some(101),
                "{client} timeout exit 101 not preserved: stderr={}",
                text(&fail_res.stderr)
            );
            has(&text(&fail_res.stderr), "[justless run-");
        }

        // c) `cargo test 2>&1 | head -n 20`
        let (pipe_cmd, _) = route(client, "cargo test 2>&1 | head -n 20");
        has(&pipe_cmd, "--window --merge --");
        let pipe_res = run_sh(&pipe_cmd, Some("0"));
        assert!(
            pipe_res.status.success(),
            "{client} pipe failed: stderr={}",
            text(&pipe_res.stderr)
        );
        let pipe_stdout = text(&pipe_res.stdout);
        let pipe_stderr = text(&pipe_res.stderr);
        assert_eq!(
            pipe_stdout.lines().count(),
            20,
            "{client} head -n 20 stdout:\n{pipe_stdout}"
        );
        has(&pipe_stdout, "cargo-out-1 ");
        has(&pipe_stdout, "cargo-err-1 ");
        has(&pipe_stderr, "[justless run-");
    }
}

#[test]
fn live_agy_edit_parse_error_bridge_lifecycle() {
    let sb = Sandbox::new();
    let lib_path = sb.root.join("src/lib.rs");
    let conv = "conv-wave2-parse-bridge";

    let edit_payload = json!({
        "cwd": sb.root,
        "workspacePaths": [sb.root],
        "conversationId": conv,
        "toolCall": {
            "id": "tc-wave2-1",
            "name": "edit_file",
            "args": {"Path": lib_path}
        }
    });
    let post_payload = json!({
        "cwd": sb.root,
        "workspacePaths": [sb.root],
        "conversationId": conv
    });

    // 1. `before-edit` snapshots the valid file and allows.
    assert_eq!(
        sb.hook_json(&["hook-agy", "before-edit"], &edit_payload),
        json!({"decision": "allow"})
    );

    // 2. Introduce a Rust syntax error and invoke `after-edit`.
    fs::write(&lib_path, "pub fn ok() -> u32 {\n    let broken = ;\n}\n").unwrap();
    assert_eq!(
        sb.hook_json(&["hook-agy", "after-edit"], &edit_payload),
        json!({})
    );

    // 3. First `post-invocation` injects the parse error into `injectSteps`.
    let first_post = sb.hook_json(&["hook-agy", "post-invocation"], &post_payload);
    let injected = first_post["injectSteps"][0]["systemMessage"]["systemMessage"]
        .as_str()
        .unwrap_or_else(|| panic!("expected injectSteps systemMessage, got: {first_post}"));
    has(injected, "Justless parse check: after this edit_file");
    has(injected, "src/lib.rs:2:18");
    has(injected, "expected an expression");
    has(injected, "The file parsed before it.");

    // 4. Second `post-invocation` returns `{}` (consumed once).
    assert_eq!(
        sb.hook_json(&["hook-agy", "post-invocation"], &post_payload),
        json!({})
    );

    // 5. Repairing the file (`before-edit` -> valid Rust -> `after-edit` -> `post-invocation`)
    //    queues no error note.
    let fix_payload = json!({
        "cwd": sb.root,
        "workspacePaths": [sb.root],
        "conversationId": conv,
        "toolCall": {
            "id": "tc-wave2-2",
            "name": "edit_file",
            "args": {"Path": lib_path}
        }
    });
    assert_eq!(
        sb.hook_json(&["hook-agy", "before-edit"], &fix_payload),
        json!({"decision": "allow"})
    );
    fs::write(
        &lib_path,
        "pub fn ok() -> u32 {\n    let fixed = 7;\n    fixed\n}\n",
    )
    .unwrap();
    assert_eq!(
        sb.hook_json(&["hook-agy", "after-edit"], &fix_payload),
        json!({})
    );
    assert_eq!(
        sb.hook_json(&["hook-agy", "post-invocation"], &post_payload),
        json!({})
    );
}

// ---------------------------------------------------------------------------
// 5. OpenCode plugin: duplicate early retirement + `session_id` forwarding
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn opencode_plugin_retires_in_window_duplicates_and_forwards_session_id() {
    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("skipped: node is not on PATH");
        return;
    }

    let sb = Sandbox::new();
    let plugin_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("integrations/opencode.js");
    let plugin_url = format!("file://{}", plugin_path.display());

    // Part A: Only 2 tool calls total (both inside keep-3) with identical >1 KiB output.
    // The plugin must invoke real `justless mask --model openai/gpt-5.5` so the earlier
    // duplicate is retired to `[justless recall ...]` while the latest stays verbatim.
    let dup_script = r#"
        const { Justless } = await import(process.env.PLUGIN);
        const hooks = await Justless({ directory: process.env.PROJECT });
        const body = "duplicate tool output line for recall verification\n".repeat(40);
        const messages = [
          { info: { role: "user", model: { providerID: "openai", modelID: "gpt-5.5" } }, parts: [] },
          { info: { role: "assistant" }, parts: [
            { type: "tool", callID: "call-1", tool: "read", state: { status: "completed", output: body } },
            { type: "tool", callID: "call-2", tool: "read", state: { status: "completed", output: body } },
          ] },
        ];
        await hooks["experimental.chat.messages.transform"]({}, { messages });
        console.log(JSON.stringify({
          first: messages[1].parts[0].state.output,
          second: messages[1].parts[1].state.output,
          body,
        }));
    "#;
    let out = Command::new("node")
        .args(["--input-type=module", "-e", dup_script])
        .env("PLUGIN", &plugin_url)
        .env("PROJECT", &sb.root)
        .env("JUSTLESS_BIN", BIN)
        .env("HOME", sb.home())
        .env("XDG_STATE_HOME", sb.home().join(".local/state"))
        .env_remove("JUSTLESS_DISABLE")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let res: Value = serde_json::from_slice(&out.stdout).unwrap();
    let first = res["first"].as_str().unwrap();
    let second = res["second"].as_str().unwrap();
    let body = res["body"].as_str().unwrap();

    assert!(
        first.starts_with("[justless recall "),
        "earlier duplicate should be retired: {first}"
    );
    assert_eq!(second, body, "latest duplicate must stay verbatim");

    // Verify exact lossless recall of the retired duplicate through the real CLI.
    let id_start = "[justless recall ".len();
    let recall_id: String = first[id_start..]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect();
    assert_eq!(recall_id.len(), 64, "bad recall stub: {first}");
    let recalled = sb
        .cmd(&["--root", sb.root.to_str().unwrap(), "recall", &recall_id])
        .output()
        .unwrap();
    assert!(recalled.status.success(), "{}", text(&recalled.stderr));
    assert_eq!(text(&recalled.stdout), body);

    // Part B: `tool.execute.after` forwards `input.sessionID` as `session_id` to
    // `hook-claude after-bash`.
    let captured_stdin = sb.base.join("after_bash_stdin.json");
    let fake_bin = sb.base.join("fake-justless-session");
    let wrapper = format!(
        "#!/bin/sh\ncat > '{}'\nprintf '{{\"hookSpecificOutput\":{{\"updatedToolOutput\":{{\"stdout\":\"compacted-ok\"}}}}}}'\n",
        captured_stdin.display()
    );
    executable(&fake_bin, &wrapper);

    let bash_script = r#"
        const { Justless } = await import(process.env.PLUGIN);
        const hooks = await Justless({ directory: process.env.PROJECT });
        const output = { output: "row\n".repeat(1500), metadata: { exit: 0 } };
        await hooks["tool.execute.after"](
          { tool: "bash", sessionID: "oc-sess-42", args: { command: "rg needle src" } },
          output,
        );
        console.log(output.output);
    "#;
    let bash_out = Command::new("node")
        .args(["--input-type=module", "-e", bash_script])
        .env("PLUGIN", &plugin_url)
        .env("PROJECT", &sb.root)
        .env("JUSTLESS_BIN", &fake_bin)
        .env_remove("JUSTLESS_DISABLE")
        .output()
        .unwrap();
    assert!(bash_out.status.success(), "{}", text(&bash_out.stderr));
    assert_eq!(text(&bash_out.stdout).trim(), "compacted-ok");

    let payload: Value =
        serde_json::from_str(&fs::read_to_string(&captured_stdin).unwrap()).unwrap();
    assert_eq!(payload["session_id"], "oc-sess-42", "{payload}");
    assert_eq!(payload["hook_event_name"], "PostToolUse", "{payload}");
}
