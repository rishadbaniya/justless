//! Commands the hooks route for the ordinary preview alone, and the progress
//! lines of common tools.
//!
//! A command listed in [`ROUTED`] has no report of its own: its run is archived
//! and shown with the clipped preview every run gets. [`PROGRESS`] names lines a
//! tool prints while it works, which the preview leaves out when the other
//! lines then fit, and the log keeps. Both lists hold invocations that build,
//! test, check, install or display. A command whose purpose is to change remote
//! state or to read secrets or a data store is not listed, nor is one that runs
//! any target the project defines, and no listed command is run for a client
//! without the client asking first. A tool the hooks already routed gets no
//! progress lines here, so its preview is unchanged.
use std::path::Path;

/// Options that follow a stream until interrupted.
const FOLLOW: &[&str] = &["-f", "--follow"];

/// The words each routed command starts with, after any package runner, and the
/// options that keep it as written.
const ROUTED: &[(&[&str], &[&str])] = &[
    // Rust
    (&["cargo", "fmt"], &[]),
    (&["cargo", "install"], &[]),
    (&["rustfmt"], &[]),
    // JavaScript and TypeScript
    (&["npm", "install"], &[]),
    (&["npm", "i"], &[]),
    (&["npm", "ci"], &[]),
    (&["npm", "outdated"], &[]),
    (&["npm", "run", "build"], &[]),
    (&["npm", "run", "lint"], &[]),
    (&["npm", "run", "typecheck"], &[]),
    (&["pnpm", "install"], &[]),
    (&["pnpm", "i"], &[]),
    (&["pnpm", "add"], &[]),
    (&["pnpm", "list"], &[]),
    (&["pnpm", "ls"], &[]),
    (&["pnpm", "outdated"], &[]),
    (&["pnpm", "build"], &[]),
    (&["pnpm", "lint"], &[]),
    (&["pnpm", "typecheck"], &[]),
    (&["pnpm", "run", "build"], &[]),
    (&["pnpm", "run", "lint"], &[]),
    (&["pnpm", "run", "typecheck"], &[]),
    (&["yarn", "install"], &[]),
    (&["yarn", "build"], &[]),
    (&["yarn", "lint"], &[]),
    (&["yarn", "typecheck"], &[]),
    (&["bun", "install"], &[]),
    (&["bun", "add"], &[]),
    (&["bun", "remove"], &[]),
    (&["bun", "build"], &[]),
    (&["bun", "run", "build"], &[]),
    (&["bun", "run", "lint"], &[]),
    (&["bun", "run", "typecheck"], &[]),
    (&["biome", "format"], &[]),
    (&["prettier"], &[]),
    (&["ng", "build"], &[]),
    (&["ng", "lint"], &[]),
    (&["next", "build"], &[]),
    (&["next", "lint"], &[]),
    (&["playwright", "test"], &["--ui", "--debug", "--headed"]),
    (&["prisma", "generate"], &[]),
    (&["prisma", "validate"], &[]),
    (&["prisma", "format"], &[]),
    (&["prisma", "migrate", "status"], &[]),
    (&["deno", "test"], &[]),
    (&["deno", "lint"], &[]),
    (&["deno", "check"], &[]),
    (&["deno", "fmt"], &[]),
    (&["deno", "compile"], &[]),
    (&["deno", "install"], &[]),
    (&["turbo", "build"], &[]),
    (&["turbo", "lint"], &[]),
    (&["turbo", "test"], &[]),
    (&["turbo", "run", "build"], &[]),
    (&["turbo", "run", "lint"], &[]),
    (&["turbo", "run", "test"], &[]),
    (&["nx", "build"], &[]),
    (&["nx", "lint"], &[]),
    (&["nx", "test"], &[]),
    // Python
    (&["black"], &[]),
    (&["python", "-m", "unittest"], &[]),
    (&["python3", "-m", "unittest"], &[]),
    (&["ruff", "format"], &[]),
    (&["sqlfluff", "lint"], &[]),
    (&["pip", "install"], &[]),
    (&["pip", "list"], &[]),
    (&["pip", "show"], &[]),
    (&["pip", "freeze"], &[]),
    (&["pip3", "install"], &[]),
    (&["pip3", "list"], &[]),
    (&["pip3", "show"], &[]),
    (&["pip3", "freeze"], &[]),
    (&["uv", "sync"], &[]),
    (&["uv", "lock"], &[]),
    (&["uv", "add"], &[]),
    (&["uv", "pip", "install"], &[]),
    (&["uv", "pip", "list"], &[]),
    (&["poetry", "install"], &[]),
    (&["poetry", "lock"], &[]),
    (&["poetry", "update"], &[]),
    // Ruby
    (&["bundle", "install"], &[]),
    (&["bundle", "update"], &[]),
    (&["rake", "test"], &[]),
    (&["rails", "test"], &[]),
    (&["rspec"], &[]),
    // PHP
    (&["php", "artisan", "test"], &[]),
    (&["php", "-l"], &[]),
    (&["phpunit"], &[]),
    (&["paratest"], &[]),
    (&["pest"], &[]),
    (&["phpstan", "analyse"], &[]),
    (&["phpstan", "analyze"], &[]),
    (&["ecs", "check"], &[]),
    (&["pint"], &[]),
    (&["composer", "install"], &[]),
    (&["composer", "update"], &[]),
    (&["composer", "require"], &[]),
    // JVM, Elixir and other builds
    (&["mvnd", "compile"], &[]),
    (&["mvnd", "test"], &[]),
    (&["mvnd", "package"], &[]),
    (&["mvnd", "verify"], &[]),
    (&["sbt", "test"], &[]),
    (&["sbt", "compile"], &[]),
    (&["sbt", "clean"], &[]),
    (&["sbt", "package"], &[]),
    (&["sbt", "assembly"], &[]),
    (&["mix", "compile"], &[]),
    (&["mix", "format"], &[]),
    (&["mix", "test"], &[]),
    (&["mix", "deps.get"], &[]),
    (&["ctest"], &[]),
    (&["bazel", "build"], &[]),
    (&["bazel", "test"], &[]),
    (&["zig", "build"], &[]),
    (&["zig", "test"], &[]),
    (&["xcodebuild"], &[]),
    (&["trunk", "build"], &[]),
    (&["pio", "run"], &["-t", "--target"]),
    (&["quarto", "render"], &[]),
    // Linters, formatters and checks
    (&["gofmt"], &[]),
    (&["shfmt"], &[]),
    (&["markdownlint"], &[]),
    (&["markdownlint-cli2"], &[]),
    (&["yamllint"], &[]),
    (&["ansible-lint"], &[]),
    (&["pre-commit", "run"], &[]),
    // Packages on the machine
    (&["brew", "install"], &[]),
    (&["brew", "upgrade"], &[]),
    (&["brew", "update"], &[]),
    // Containers and infrastructure, display and dry runs only
    (&["docker", "images"], &[]),
    (&["docker", "image", "ls"], &[]),
    (&["docker", "build"], &["--push"]),
    (&["docker", "compose", "build"], &["--push"]),
    (&["podman", "images"], &[]),
    (&["podman", "image", "ls"], &[]),
    (&["podman", "build"], &["--push"]),
    (&["kubectl", "describe"], &[]),
    (&["oc", "get"], &["-w", "--watch"]),
    (&["oc", "describe"], &[]),
    (&["oc", "status"], &[]),
    (&["helm", "list"], &[]),
    (&["helm", "status"], &[]),
    (&["helm", "history"], &[]),
    (&["helm", "lint"], &[]),
    (&["terraform", "plan"], &[]),
    (&["terraform", "validate"], &[]),
    (&["terraform", "fmt"], &[]),
    (&["terraform", "init"], &[]),
    (&["tofu", "plan"], &[]),
    (&["tofu", "validate"], &[]),
    (&["tofu", "fmt"], &[]),
    (&["tofu", "init"], &[]),
    (&["pulumi", "preview"], &[]),
    (&["pulumi", "stack", "ls"], &[]),
    // The machine
    (&["df"], &[]),
    (&["du"], &[]),
    (&["ps"], &[]),
    (&["systemctl", "status"], &[]),
    (&["journalctl"], FOLLOW),
    // GitHub and GitLab, lists and views
    (&["gh", "pr", "list"], &[]),
    (&["gh", "pr", "view"], &["-w", "--web"]),
    (&["gh", "pr", "checks"], &["--watch"]),
    (&["gh", "pr", "diff"], &[]),
    (&["gh", "issue", "list"], &[]),
    (&["gh", "issue", "view"], &["-w", "--web"]),
    (&["gh", "run", "list"], &[]),
    (&["gh", "run", "view"], &["-w", "--web"]),
    (&["gh", "repo", "view"], &["-w", "--web"]),
    (&["gh", "release", "list"], &[]),
    (&["gh", "release", "view"], &["-w", "--web"]),
    (&["glab", "mr", "list"], &[]),
    (&["glab", "mr", "view"], &["-w", "--web"]),
    (&["glab", "issue", "list"], &[]),
    (&["glab", "issue", "view"], &["-w", "--web"]),
    (&["glab", "ci", "status"], &["-l", "--live"]),
    (&["glab", "ci", "list"], &[]),
    (&["glab", "pipeline", "list"], &[]),
    (&["glab", "release", "list"], &[]),
];

/// Lines a tool prints while it works, matched against each line without its
/// surrounding spaces. A line that looks like a failure or a warning is kept
/// whatever matches here, and so are the four lines after it.
const PROGRESS: &[(&[&str], &str)] = &[
    (
        &["cargo", "install"],
        r"^(Compiling|Downloaded|Downloading|Updating|Locking|Adding|Fresh|Checking) ",
    ),
    (
        &["npm", "install"],
        r"^(\d+ packages? (are|is) looking for funding|run `npm fund` for details)$",
    ),
    (
        &["npm", "i"],
        r"^(\d+ packages? (are|is) looking for funding|run `npm fund` for details)$",
    ),
    (
        &["npm", "ci"],
        r"^(\d+ packages? (are|is) looking for funding|run `npm fund` for details)$",
    ),
    (&["pnpm", "install"], r"^(Progress: resolved \d|\++$)"),
    (&["pnpm", "i"], r"^(Progress: resolved \d|\++$)"),
    (&["pnpm", "add"], r"^(Progress: resolved \d|\++$)"),
    (
        &["pip", "install"],
        r"^(Collecting |Downloading |Using cached |Requirement already satisfied|Preparing metadata|Installing build dependencies|Getting requirements to build|Building wheel|Created wheel|Stored in directory)",
    ),
    (
        &["pip3", "install"],
        r"^(Collecting |Downloading |Using cached |Requirement already satisfied|Preparing metadata|Installing build dependencies|Getting requirements to build|Building wheel|Created wheel|Stored in directory)",
    ),
    (
        &["poetry", "install"],
        r"^[-•] (Installing|Updating|Downgrading|Removing) \S+ \(",
    ),
    (
        &["poetry", "update"],
        r"^[-•] (Installing|Updating|Downgrading|Removing) \S+ \(",
    ),
    (
        &["bundle", "install"],
        r"^(Using|Fetching|Installing) \S+ \d\S*( \([^)]*\))?( with native extensions)?$",
    ),
    (
        &["bundle", "update"],
        r"^(Using|Fetching|Installing) \S+ \d\S*( \([^)]*\))?( with native extensions)?$",
    ),
    (
        &["composer", "install"],
        r"^- (Downloading|Installing|Syncing|Locking|Upgrading|Downgrading|Removing) \S+ \(",
    ),
    (
        &["composer", "update"],
        r"^- (Downloading|Installing|Syncing|Locking|Upgrading|Downgrading|Removing) \S+ \(",
    ),
    (
        &["composer", "require"],
        r"^- (Downloading|Installing|Syncing|Locking|Upgrading|Downgrading|Removing) \S+ \(",
    ),
    (
        &["brew", "install"],
        r"^(==> (Downloading|Fetching|Pouring) |Already downloaded: |#+ +\d+(\.\d+)?%)",
    ),
    (
        &["brew", "upgrade"],
        r"^(==> (Downloading|Fetching|Pouring) |Already downloaded: |#+ +\d+(\.\d+)?%)",
    ),
    (
        &["terraform", "plan"],
        r": (Refreshing state\.\.\.|Reading\.\.\.|Still reading\.\.\.|Read complete after )",
    ),
    (
        &["tofu", "plan"],
        r": (Refreshing state\.\.\.|Reading\.\.\.|Still reading\.\.\.|Read complete after )",
    ),
    (
        &["terraform", "init"],
        r"^- (Finding|Installing|Installed|Reusing previous version of|Using previously-installed) ",
    ),
    (
        &["tofu", "init"],
        r"^- (Finding|Installing|Installed|Reusing previous version of|Using previously-installed) ",
    ),
    (&["deno"], r"^(Download https?://|Check file://)"),
    (&["phpunit"], r"^\.+ +\d+ / \d+ \( *\d+%\)$"),
    (&["paratest"], r"^\.+ +\d+ / \d+ \( *\d+%\)$"),
    (
        &["ctest"],
        r"^(Start +\d+: |\d+/\d+ Test +#\d+: .*\bPassed\b)",
    ),
    (
        &["pre-commit", "run"],
        r"\.{3,}(\(no files to check\))?(Passed|Skipped)$",
    ),
    (
        &["sbt"],
        r"^\[info\] (welcome to sbt|loading |set current project to|[Rr]esolving |Fetching |Fetched |Updating |Updated |[Cc]ompiling \d+ |done compiling)",
    ),
    (
        &["docker", "build"],
        r"^(?:#\d+ (?:CACHED|DONE \d|\[internal\] |transferring |load (?:build|metadata|transfer)|sha256:[0-9a-f]|exporting (?:layers|manifest|config))|---> (?:Using cache|[0-9a-f]{12})$)",
    ),
    (
        &["docker", "compose", "build"],
        r"^(?:#\d+ (?:CACHED|DONE \d|\[internal\] |transferring |load (?:build|metadata|transfer)|sha256:[0-9a-f]|exporting (?:layers|manifest|config))|---> (?:Using cache|[0-9a-f]{12})$)",
    ),
    (
        &["uv", "sync"],
        r"^(?:Downloading |Building |Prepared \d+ packages?|Fetching |Unpacking )",
    ),
    (
        &["uv", "pip", "install"],
        r"^(?:Downloading |Building |Prepared \d+ packages?|Fetching |Unpacking )",
    ),
    (
        &["uv", "add"],
        r"^(?:Downloading |Building |Prepared \d+ packages?|Fetching |Unpacking )",
    ),
    (
        &["uv", "lock"],
        r"^(?:Downloading |Building |Prepared \d+ packages?|Fetching |Unpacking )",
    ),
    (
        &["yarn", "install"],
        r"^(?:\[[1-4]/4\] (?:Resolving|Fetching|Linking|Building) |info Fetching |➤ YN0000: [┌│└] (?:Resolution|Fetch|Link) step)",
    ),
    (
        &["bun", "install"],
        r"^(?:Resolving dependencies|Resolved, downloaded and extracted \[\d+\]|Saved lockfile)$",
    ),
    (
        &["bun", "add"],
        r"^(?:Resolving dependencies|Resolved, downloaded and extracted \[\d+\]|Saved lockfile)$",
    ),
    (
        &["xcodebuild"],
        r"^(?:Compile(?:Swift|C|AssetCatalog|Storyboard|XIB)|Swift(?:Compile|Driver|EmitModule|MergeGeneratedHeaders)|Ld |ProcessInfoPlistFile |CpResource |CopySwiftLibs |Touch |WriteAuxiliaryFile |MkDir |CreateBuildDirectory |PhaseScriptExecution |CodeSign |Validate |RegisterExecutionPolicyException |SymLink |Libtool |Ditto )",
    ),
    (
        &["mix", "compile"],
        r"^(?:Compiling \d+ files? \(|Generated \S+ app$)",
    ),
    (
        &["mix", "deps.get"],
        r"^(?:\* Getting |Resolving Hex dependencies)",
    ),
    (
        &["next", "build"],
        r"^(?:Creating an optimized production build|Collecting page data|Generating static pages \(\d+/\d+\)|Finalizing page optimization|Collecting build traces)",
    ),
    (
        &["turbo", "build"],
        r"^(?:• Packages in scope:|• Running \S+ in \d+ packages?|• Remote caching |.+: cache hit, replaying logs )",
    ),
    (
        &["turbo", "run", "build"],
        r"^(?:• Packages in scope:|• Running \S+ in \d+ packages?|• Remote caching |.+: cache hit, replaying logs )",
    ),
];

/// The entry with the longest command that `words` start with, reading the
/// program by its file name and skipping package runners such as `npx`.
fn longest<'a, T>(table: &'a [(&'static [&'static str], T)], words: &[&str]) -> Option<&'a T> {
    let words = crate::filters::unwrap_tool(words);
    let (program, args) = words.split_first()?;
    let program = Path::new(program).file_name()?.to_str()?;
    table
        .iter()
        .filter(|(command, _)| command[0] == program && args.starts_with(&command[1..]))
        .max_by_key(|(command, _)| command.len())
        .map(|(_, value)| value)
}

/// Whether the hooks route this command for its preview alone.
pub fn listed(words: &[&str]) -> bool {
    longest(ROUTED, words).is_some()
}

/// Whether the hooks may route this invocation: a listed command with none of
/// the options that keep it as written.
pub fn routes(words: &[&str]) -> bool {
    let Some(unwrapped) = crate::wrappers::hook_effective(words) else {
        return false;
    };
    let refs: Vec<&str> = unwrapped.iter().map(String::as_str).collect();
    longest(ROUTED, &refs).is_some_and(|except| {
        !refs
            .iter()
            .any(|word| except.contains(&word.split_once('=').map_or(*word, |(flag, _)| flag)))
    })
}

/// The progress lines this command's preview leaves out.
pub fn progress(words: &[&str]) -> Option<&'static str> {
    longest(PROGRESS, words).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filters::{Selection, approvable, legacy_hook, profile};
    use std::ffi::OsString;

    #[test]
    fn every_listed_command_is_routed_and_never_approved_for_the_client() {
        for (command, except) in ROUTED {
            let line = command.join(" ");
            assert!(crate::agents::supported_words(command), "{line}");
            assert!(!approvable(command), "{line} runs without asking");
            // A command with a report or rules of its own stays out of this list.
            assert!(
                !legacy_hook(command) && profile(command).is_none(),
                "{line}"
            );
            for option in *except {
                let mut words = command.to_vec();
                words.push(option);
                assert!(!routes(&words), "{line} {option}");
            }
        }
        let mut sorted: Vec<_> = ROUTED.iter().map(|(command, _)| command).collect();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ROUTED.len(), "a command is listed twice");
    }

    #[test]
    fn commands_that_change_remote_state_or_print_secrets_stay_as_written() {
        for line in [
            "terraform apply",
            "terraform destroy",
            "pulumi up",
            "pulumi stack rm dev",
            "kubectl apply -f x.yaml",
            "kubectl delete pod web",
            "helm install web ./chart",
            "docker run alpine",
            "docker exec web env",
            "docker compose up",
            "docker compose logs -f",
            "journalctl --follow",
            "prisma migrate deploy",
            "npm run dev",
            "npm publish",
            "sbt run",
            "just deploy",
            "mvnd deploy",
            "nx run-many --target=deploy",
            "docker build --push .",
            "podman build --push .",
            "podman run alpine",
            "podman logs -f web",
            "pio run -t upload",
            "helm template web ./chart",
            "deno run main.ts",
            "sops -d secrets.yaml",
            "psql -c select",
            "aws s3 ls",
            "gcloud compute instances list",
            "curl https://example.com",
            "ssh host uptime",
            "env",
            "git push",
            "gh pr merge 1",
            "gh pr view 1 --web",
        ] {
            let words: Vec<&str> = line.split(' ').collect();
            assert!(!crate::agents::supported_words(&words), "{line}");
        }
    }

    #[test]
    fn a_package_runner_or_a_path_selects_the_same_command() {
        for line in [
            "npx prettier --check .",
            "pnpm exec biome format .",
            "bundle exec rspec",
            "poetry run sqlfluff lint .",
            "vendor/bin/phpunit",
            "./vendor/bin/pest",
            "bin/rails test",
            "python3 -m pip install -r requirements.txt",
            "python3 -m unittest discover -s tests",
            "python -m unittest tests/test_app.py",
            "poetry run python -m unittest",
            "python3 -m black --check src",
            "bun run typecheck",
            "bazel build //...",
            "bazel test //pkg:all",
            "zig build",
            "zig test src/main.zig",
            "podman build -t app .",
            "podman images",
            "podman image ls",
            "gofmt -l .",
            "shfmt -d scripts/",
            "rustfmt --check src/main.rs",
        ] {
            let words: Vec<&str> = line.split(' ').collect();
            assert!(routes(&words), "{line}");
        }
    }

    /// Whether the preview of `command` leaves `line` out.
    fn omitted(command: &str, line: &str) -> bool {
        let args: Vec<OsString> = command.split(' ').map(OsString::from).collect();
        let selection = Selection::for_run(&args, &[]).unwrap();
        selection
            .omit
            .is_some_and(|omit| omit.is_match(line.trim()))
    }

    // These lines are written from each tool's usual output, not captured from a run.
    #[test]
    fn progress_lines_are_left_out_and_results_are_kept() {
        for (command, dropped, kept) in [
            (
                "cargo install ripgrep",
                "   Compiling memchr v2.7.4",
                "   Installed package `ripgrep v14.1.0` (executable `rg`)",
            ),
            (
                "npm install",
                "12 packages are looking for funding",
                "added 120 packages, and audited 121 packages in 3s",
            ),
            (
                "pnpm install",
                "Progress: resolved 412, reused 410, downloaded 2, added 0",
                "Done in 2.1s",
            ),
            (
                "pip install requests",
                "Collecting requests",
                "Successfully installed requests-2.32.3",
            ),
            (
                "python3 -m pip install requests",
                "  Using cached requests-2.32.3-py3-none-any.whl (64 kB)",
                "Installing collected packages: urllib3, idna, requests",
            ),
            (
                "poetry install",
                "  - Installing certifi (2024.8.30)",
                "Installing the current project: demo (0.1.0)",
            ),
            (
                "bundle install",
                "Using rake 13.2.1",
                "Bundle complete! 5 Gemfile dependencies, 31 gems now installed.",
            ),
            (
                "bundle install",
                "Installing nokogiri 1.16.7 (arm64-darwin) with native extensions",
                "Fetching gem metadata from https://rubygems.org/.........",
            ),
            (
                "composer install",
                "  - Installing symfony/console (v7.1.5): Extracting archive",
                "Generating optimized autoload files",
            ),
            (
                "brew install jq",
                "==> Downloading https://ghcr.io/v2/homebrew/core/jq/manifests/1.7.1",
                "==> Summary",
            ),
            (
                "terraform plan",
                "aws_instance.web: Refreshing state... [id=i-0abc]",
                "Plan: 1 to add, 0 to change, 0 to destroy.",
            ),
            (
                "tofu plan",
                "data.aws_ami.base: Read complete after 0s [id=ami-1]",
                "  # aws_instance.web will be created",
            ),
            (
                "terraform init",
                "- Installing hashicorp/aws v5.70.0...",
                "Terraform has been successfully initialized!",
            ),
            (
                "deno test",
                "Download https://deno.land/std@0.224.0/assert/mod.ts",
                "ok | 3 passed | 0 failed (12ms)",
            ),
            (
                "phpunit",
                "...............................................................  63 / 120 ( 52%)",
                "OK (120 tests, 310 assertions)",
            ),
            (
                "ctest",
                " 1/12 Test  #1: parse_empty ......................   Passed    0.01 sec",
                "100% tests passed, 0 tests failed out of 12",
            ),
            (
                "pre-commit run",
                "trim trailing whitespace.................................................Passed",
                "ruff.....................................................................Failed",
            ),
            (
                "sbt test",
                "[info] welcome to sbt 1.10.2 (Eclipse Adoptium Java 21.0.4)",
                "[info] Passed: Total 12, Failed 0, Errors 0, Passed 12",
            ),
            (
                "docker build .",
                "#8 CACHED",
                "#12 naming to docker.io/library/app:latest",
            ),
            (
                "uv sync",
                "Downloading httpx (76.2 KiB)",
                "Installed 14 packages in 28ms",
            ),
            (
                "yarn install",
                "[2/4] Fetching packages...",
                "Done in 3.12s.",
            ),
            (
                "bun install",
                "Resolving dependencies",
                "42 packages installed [180.00ms]",
            ),
            (
                "xcodebuild build",
                "CompileSwift normal arm64 /src/App.swift",
                "** BUILD SUCCEEDED **",
            ),
            (
                "mix compile",
                "Compiling 42 files (.ex)",
                "warning: unused variable x",
            ),
            (
                "next build",
                "Generating static pages (12/40)",
                "Route (app)  Size  First Load JS",
            ),
            (
                "turbo build",
                "web:build: cache hit, replaying logs 8a7b6c",
                "Tasks:    4 successful, 4 total",
            ),
        ] {
            assert!(omitted(command, dropped), "{command}: {dropped}");
            assert!(!omitted(command, kept), "{command}: {kept}");
        }
    }

    #[test]
    fn every_progress_rule_compiles_and_matches_no_empty_or_failing_line() {
        for (command, pattern) in PROGRESS {
            // A tool the hooks already routed keeps the preview it had.
            assert!(
                !legacy_hook(command) && profile(command).is_none(),
                "{command:?}"
            );
            let omit = regex::Regex::new(pattern).unwrap();
            assert!(!omit.is_match(""), "{command:?}");
            for line in [
                "error: build failed",
                "FAILED tests/test_app.py::test_x",
                "warning: unused",
            ] {
                assert!(
                    !omit.is_match(line) || crate::filters::diagnostic(line),
                    "{command:?}: {line}"
                );
            }
        }
    }

    #[test]
    fn a_rule_from_the_project_settings_replaces_the_built_in_one() {
        let args: Vec<OsString> = ["pip", "install", "x"].iter().map(OsString::from).collect();
        assert!(Selection::for_run(&args, &[]).unwrap().built_in);
        let rules = [crate::filters::Rule {
            command: vec!["pip".into()],
            omit: "^Looking in indexes".into(),
        }];
        let selection = Selection::for_run(&args, &rules).unwrap();
        assert!(!selection.built_in);
        let omit = selection.omit.unwrap();
        assert!(omit.is_match("Looking in indexes: https://pypi.org/simple"));
        assert!(!omit.is_match("Collecting x"));
    }
}
