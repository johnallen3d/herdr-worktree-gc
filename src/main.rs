use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use wait_timeout::ChildExt;

const PLUGIN_ID: &str = "worktree-gc";
const DEFAULT_DEBOUNCE_SECONDS: u64 = 300;
const DEFAULT_FETCH_TIMEOUT_SECONDS: u64 = 60;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default)]
struct Config {
    auto_remove: bool,
    debounce_seconds: u64,
    fetch_timeout_seconds: u64,
    check_processes: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            auto_remove: false,
            debounce_seconds: DEFAULT_DEBOUNCE_SECONDS,
            fetch_timeout_seconds: DEFAULT_FETCH_TIMEOUT_SECONDS,
            check_processes: true,
        }
    }
}

impl Config {
    fn load(config_dir: &Path) -> Result<Self, String> {
        let path = config_dir.join("config.toml");
        if !path.exists() {
            return Ok(Self::default());
        }
        let contents = fs::read_to_string(&path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        toml::from_str(&contents).map_err(|error| format!("invalid {}: {error}", path.display()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandResult {
    returncode: i32,
    stdout: String,
    stderr: String,
}

trait Runner {
    fn run(
        &self,
        args: &[String],
        timeout: Option<Duration>,
        extra_env: &[(&str, &str)],
    ) -> CommandResult;
}

struct CommandRunner;

impl Runner for CommandRunner {
    fn run(
        &self,
        args: &[String],
        timeout: Option<Duration>,
        extra_env: &[(&str, &str)],
    ) -> CommandResult {
        if args.is_empty() {
            return CommandResult {
                returncode: 127,
                stdout: String::new(),
                stderr: "empty command".into(),
            };
        }
        let mut command = Command::new(&args[0]);
        command
            .args(&args[1..])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return CommandResult {
                    returncode: 127,
                    stdout: String::new(),
                    stderr: error.to_string(),
                };
            }
        };

        let stdout_reader = child.stdout.take().map(|mut pipe| {
            thread::spawn(move || {
                let mut output = String::new();
                let _ = pipe.read_to_string(&mut output);
                output
            })
        });
        let stderr_reader = child.stderr.take().map(|mut pipe| {
            thread::spawn(move || {
                let mut output = String::new();
                let _ = pipe.read_to_string(&mut output);
                output
            })
        });

        let mut timed_out = false;
        let status = match timeout {
            Some(limit) => match child.wait_timeout(limit) {
                Ok(Some(status)) => Some(status),
                Ok(None) => {
                    timed_out = true;
                    let _ = child.kill();
                    child.wait().ok()
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return CommandResult {
                        returncode: 127,
                        stdout: String::new(),
                        stderr: error.to_string(),
                    };
                }
            },
            None => child.wait().ok(),
        };

        let stdout = stdout_reader
            .and_then(|reader| reader.join().ok())
            .unwrap_or_default();
        let mut stderr = stderr_reader
            .and_then(|reader| reader.join().ok())
            .unwrap_or_default();
        if timed_out && stderr.trim().is_empty() {
            stderr = "command timed out".into();
        }
        CommandResult {
            returncode: if timed_out {
                124
            } else {
                status.and_then(|value| value.code()).unwrap_or(127)
            },
            stdout,
            stderr,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Worktree {
    path: PathBuf,
    branch_ref: Option<String>,
    #[allow(dead_code)]
    head: Option<String>,
    is_main: bool,
    detached: bool,
    #[allow(dead_code)]
    prunable: bool,
}

impl Worktree {
    fn branch(&self) -> Option<&str> {
        self.branch_ref
            .as_deref()
            .and_then(|value| value.strip_prefix("refs/heads/"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Workspace {
    id: String,
    checkout_path: Option<PathBuf>,
    repo_root: Option<PathBuf>,
    focused: bool,
    has_agent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pane {
    id: String,
    workspace_id: String,
    cwd: Option<PathBuf>,
    foreground_cwd: Option<PathBuf>,
    focused: bool,
    has_agent: bool,
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq)]
struct State {
    #[serde(default)]
    last_fetch: BTreeMap<String, f64>,
}

impl State {
    fn load(path: &Path) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|contents| serde_json::from_str(&contents).ok())
            .unwrap_or_default()
    }

    fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let temporary = path.with_extension(format!("tmp.{}", std::process::id()));
        let contents = serde_json::to_string(self).map_err(|error| error.to_string())?;
        fs::write(&temporary, contents).map_err(|error| error.to_string())?;
        fs::rename(&temporary, path).map_err(|error| error.to_string())
    }
}

struct RunLock {
    path: PathBuf,
    acquired: bool,
}

impl RunLock {
    fn acquire(path: PathBuf) -> Self {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        for _ in 0..2 {
            match fs::create_dir(&path) {
                Ok(()) => {
                    if fs::write(path.join("pid"), std::process::id().to_string()).is_ok() {
                        return Self {
                            path,
                            acquired: true,
                        };
                    }
                    let _ = fs::remove_dir_all(&path);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if lock_owner_is_alive(&path) {
                        break;
                    }
                    let _ = fs::remove_dir_all(&path);
                }
                Err(_) => break,
            }
        }
        Self {
            path,
            acquired: false,
        }
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        if self.acquired {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn lock_owner_is_alive(path: &Path) -> bool {
    if let Ok(contents) = fs::read_to_string(path.join("pid"))
        && let Ok(pid) = contents.trim().parse::<i32>()
    {
        // SAFETY: kill(pid, 0) sends no signal and only checks process existence/permission.
        let result = unsafe { libc::kill(pid, 0) };
        if result == 0 {
            return true;
        }
        return std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH);
    }
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age < Duration::from_secs(3600))
}

struct Logger {
    trigger: String,
}

impl Logger {
    fn new(trigger: impl Into<String>) -> Self {
        Self {
            trigger: trigger.into(),
        }
    }

    fn emit(&self, level: &str, action: &str, details: &Value) {
        let fields = details
            .as_object()
            .into_iter()
            .flat_map(|object| object.iter())
            .filter(|(_, value)| !value.is_null())
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join(" ");
        if fields.is_empty() {
            println!("level={level} trigger={} action={action}", self.trigger);
        } else {
            println!(
                "level={level} trigger={} action={action} {fields}",
                self.trigger
            );
        }
    }
}

struct WorktreeGc<'a> {
    runner: &'a dyn Runner,
    config: Config,
    state_path: PathBuf,
    logger: &'a Logger,
    herdr_bin: String,
    wt_bin: String,
    now: fn() -> f64,
}

impl WorktreeGc<'_> {
    #[allow(clippy::too_many_lines)]
    fn run(&self, remove: bool, force_fetch: bool, event_json: &str, context_json: &str) -> i32 {
        let workspace_result = self.command(&[&self.herdr_bin, "workspace", "list"]);
        if workspace_result.returncode != 0 {
            self.logger.emit(
                "error",
                "workspace-list-failed",
                &json!({"error": message(&workspace_result)}),
            );
            return 1;
        }
        let workspaces = match parse_workspaces(&workspace_result.stdout) {
            Ok(value) => value,
            Err(error) => {
                self.logger
                    .emit("error", "workspace-list-invalid", &json!({"error": error}));
                return 1;
            }
        };

        let pane_result = self.command(&[&self.herdr_bin, "pane", "list"]);
        if pane_result.returncode != 0 {
            self.logger.emit(
                "error",
                "pane-list-failed",
                &json!({"error": message(&pane_result)}),
            );
            return 1;
        }
        let panes = match parse_panes(&pane_result.stdout) {
            Ok(value) => value,
            Err(error) => {
                self.logger
                    .emit("error", "pane-list-invalid", &json!({"error": error}));
                return 1;
            }
        };

        let mut repo_hints = event_repo_hints(event_json);
        let protected_paths = event_repo_hints(context_json);
        repo_hints.extend(protected_paths.iter().cloned());
        for pane in &panes {
            repo_hints.extend(
                [pane.cwd.clone(), pane.foreground_cwd.clone()]
                    .into_iter()
                    .flatten(),
            );
        }
        let repos = self.discover_repositories(&workspaces, &repo_hints);
        if repos.is_empty() {
            self.logger.emit(
                "info",
                "complete",
                &json!({"repositories": 0, "candidates": 0, "removed": 0}),
            );
            return 0;
        }

        let mut state = State::load(&self.state_path);
        let mut candidates = 0;
        let mut removed = 0;
        for repo_root in &repos {
            let Some(repo_key) = self.repo_key(repo_root) else {
                continue;
            };
            if !self.fetch(repo_root, &repo_key, &mut state, force_fetch) {
                continue;
            }
            for worktree in self.worktrees(repo_root) {
                if let Some(reason) =
                    self.skip_reason(&worktree, &workspaces, &panes, &protected_paths)
                {
                    self.logger.emit(
                        "info",
                        "skip",
                        &json!({
                            "repo": repo_root,
                            "worktree": worktree.path,
                            "branch": worktree.branch(),
                            "reason": reason,
                        }),
                    );
                    continue;
                }
                let Some(upstream) = self.gone_upstream(repo_root, &worktree) else {
                    continue;
                };
                candidates += 1;
                if !remove {
                    self.logger.emit(
                        "info",
                        "candidate",
                        &json!({
                            "repo": repo_root,
                            "worktree": worktree.path,
                            "branch": worktree.branch(),
                            "upstream": upstream,
                            "dry_run": true,
                        }),
                    );
                } else if self.remove_worktree(repo_root, &worktree, &upstream) {
                    removed += 1;
                }
            }
        }
        if let Err(error) = state.save(&self.state_path) {
            self.logger
                .emit("warning", "state-save-failed", &json!({"error": error}));
        }
        self.logger.emit(
            "info",
            "complete",
            &json!({
                "repositories": repos.len(),
                "candidates": candidates,
                "removed": removed,
                "dry_run": !remove,
            }),
        );
        0
    }

    fn command(&self, args: &[&str]) -> CommandResult {
        self.runner.run(
            &args
                .iter()
                .map(|value| (*value).to_owned())
                .collect::<Vec<_>>(),
            None,
            &[],
        )
    }

    fn discover_repositories(
        &self,
        workspaces: &[Workspace],
        repo_hints: &[PathBuf],
    ) -> Vec<PathBuf> {
        let mut hints = repo_hints.to_vec();
        for workspace in workspaces {
            if let Some(path) = &workspace.repo_root {
                hints.push(path.clone());
            }
            if let Some(path) = &workspace.checkout_path {
                hints.push(path.clone());
            }
        }
        let mut seen = HashSet::new();
        let mut repositories = Vec::new();
        for hint in hints {
            if !hint.exists() {
                continue;
            }
            let result = self.command_owned(&[
                "git".into(),
                "-C".into(),
                hint.to_string_lossy().into_owned(),
                "worktree".into(),
                "list".into(),
                "--porcelain".into(),
                "-z".into(),
            ]);
            if result.returncode != 0 {
                continue;
            }
            let worktrees = parse_worktrees(&result.stdout);
            let Some(first) = worktrees.first() else {
                continue;
            };
            let root = resolve_path(&first.path);
            if let Some(key) = self.repo_key(&root)
                && seen.insert(key)
            {
                repositories.push(root);
            }
        }
        repositories.sort();
        repositories
    }

    fn command_owned(&self, args: &[String]) -> CommandResult {
        self.runner.run(args, None, &[])
    }

    fn repo_key(&self, repo_root: &Path) -> Option<String> {
        let result = self.command_owned(&[
            "git".into(),
            "-C".into(),
            repo_root.to_string_lossy().into_owned(),
            "rev-parse".into(),
            "--path-format=absolute".into(),
            "--git-common-dir".into(),
        ]);
        (result.returncode == 0).then(|| {
            resolve_path(Path::new(result.stdout.trim()))
                .to_string_lossy()
                .into_owned()
        })
    }

    fn fetch(&self, repo_root: &Path, repo_key: &str, state: &mut State, force: bool) -> bool {
        let age = (self.now)() - state.last_fetch.get(repo_key).copied().unwrap_or(0.0);
        let debounce = Duration::from_secs(self.config.debounce_seconds).as_secs_f64();
        if !force && age >= 0.0 && age < debounce {
            self.logger.emit(
                "info",
                "fetch-debounced",
                &json!({"repo": repo_root, "age_seconds": age.round()}),
            );
            return true;
        }
        let result = self.runner.run(
            &[
                "git".into(),
                "-C".into(),
                repo_root.to_string_lossy().into_owned(),
                "fetch".into(),
                "--all".into(),
                "--prune".into(),
                "--no-recurse-submodules".into(),
            ],
            Some(Duration::from_secs(self.config.fetch_timeout_seconds)),
            &[("GIT_TERMINAL_PROMPT", "0")],
        );
        if result.returncode != 0 {
            self.logger.emit(
                "warning",
                "fetch-failed",
                &json!({"repo": repo_root, "error": message(&result)}),
            );
            return false;
        }
        state.last_fetch.insert(repo_key.into(), (self.now)());
        self.logger
            .emit("info", "fetch-complete", &json!({"repo": repo_root}));
        true
    }

    fn worktrees(&self, repo_root: &Path) -> Vec<Worktree> {
        let result = self.command_owned(&[
            "git".into(),
            "-C".into(),
            repo_root.to_string_lossy().into_owned(),
            "worktree".into(),
            "list".into(),
            "--porcelain".into(),
            "-z".into(),
        ]);
        if result.returncode != 0 {
            self.logger.emit(
                "warning",
                "worktree-list-failed",
                &json!({"repo": repo_root, "error": message(&result)}),
            );
            return Vec::new();
        }
        parse_worktrees(&result.stdout)
    }

    fn skip_reason(
        &self,
        worktree: &Worktree,
        workspaces: &[Workspace],
        panes: &[Pane],
        protected_paths: &[PathBuf],
    ) -> Option<String> {
        if worktree.is_main {
            return Some("main-checkout".into());
        }
        if worktree.detached || worktree.branch().is_none() {
            return Some("detached".into());
        }
        if !worktree.path.exists() {
            return Some("missing".into());
        }
        let target = resolve_path(&worktree.path);
        if protected_paths
            .iter()
            .any(|path| is_under(&resolve_path(path), &target))
        {
            return Some("current-worktree".into());
        }
        for workspace in workspaces {
            if workspace
                .checkout_path
                .as_ref()
                .is_some_and(|path| same_path(path, &target))
            {
                if workspace.focused {
                    return Some("current-worktree".into());
                }
                if workspace.has_agent {
                    return Some("active-agent".into());
                }
            }
        }
        for pane in panes {
            let inside = [&pane.cwd, &pane.foreground_cwd]
                .into_iter()
                .flatten()
                .any(|path| is_under(&resolve_path(path), &target));
            if !inside {
                continue;
            }
            if pane.focused {
                return Some("current-worktree".into());
            }
            if pane.has_agent {
                return Some("active-agent".into());
            }
        }
        if self.config.check_processes {
            match processes_under(&target, self.runner) {
                Ok(pids) if !pids.is_empty() => {
                    let shown = pids
                        .iter()
                        .take(5)
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(",");
                    return Some(format!("active-processes:{shown}"));
                }
                Err(_) => return Some("process-check-unavailable".into()),
                _ => {}
            }
        }
        None
    }

    fn gone_upstream(&self, repo_root: &Path, worktree: &Worktree) -> Option<String> {
        let branch_ref = worktree.branch_ref.as_ref()?;
        let result = self.command_owned(&[
            "git".into(),
            "-C".into(),
            repo_root.to_string_lossy().into_owned(),
            "for-each-ref".into(),
            "--format=%(upstream)".into(),
            branch_ref.clone(),
        ]);
        if result.returncode != 0 {
            return None;
        }
        let upstream = result.stdout.trim().to_owned();
        if upstream.is_empty() {
            return None;
        }
        let exists = self.command_owned(&[
            "git".into(),
            "-C".into(),
            repo_root.to_string_lossy().into_owned(),
            "show-ref".into(),
            "--verify".into(),
            "--quiet".into(),
            upstream.clone(),
        ]);
        match exists.returncode {
            0 => None,
            1 => Some(upstream),
            _ => {
                self.logger.emit(
                    "warning",
                    "upstream-check-failed",
                    &json!({
                        "repo": repo_root,
                        "branch": worktree.branch(),
                        "upstream": upstream,
                        "error": message(&exists),
                    }),
                );
                None
            }
        }
    }

    fn remove_worktree(&self, repo_root: &Path, worktree: &Worktree, upstream: &str) -> bool {
        // Deliberately omit force/reap flags. Worktrunk remains the safety authority.
        let result = self.command_owned(&[
            self.wt_bin.clone(),
            "-C".into(),
            repo_root.to_string_lossy().into_owned(),
            "remove".into(),
            "--foreground".into(),
            "--format=json".into(),
            "--yes".into(),
            worktree.path.to_string_lossy().into_owned(),
        ]);
        if result.returncode != 0 {
            self.logger.emit(
                "warning",
                "remove-refused",
                &json!({
                    "repo": repo_root,
                    "worktree": worktree.path,
                    "branch": worktree.branch(),
                    "upstream": upstream,
                    "error": message(&result),
                }),
            );
            return false;
        }
        self.logger.emit(
            "info",
            "removed",
            &json!({
                "repo": repo_root,
                "worktree": worktree.path,
                "branch": worktree.branch(),
                "upstream": upstream,
                "branch_outcome": worktrunk_outcome(&result.stdout),
            }),
        );
        self.close_stale_ui(&worktree.path);
        true
    }

    #[allow(clippy::too_many_lines)]
    fn close_stale_ui(&self, path: &Path) {
        let result = self.command(&[&self.herdr_bin, "workspace", "list"]);
        if result.returncode != 0 {
            self.logger.emit(
                "warning",
                "workspace-refresh-failed",
                &json!({"error": message(&result)}),
            );
            return;
        }
        let Ok(workspaces) = parse_workspaces(&result.stdout) else {
            return;
        };
        let mut closed_workspaces = HashSet::new();
        for workspace in workspaces {
            if !workspace
                .checkout_path
                .as_ref()
                .is_some_and(|checkout| same_path(checkout, path))
            {
                continue;
            }
            if workspace.has_agent {
                self.logger.emit(
                    "warning",
                    "workspace-close-skipped",
                    &json!({
                        "workspace": workspace.id,
                        "reason": "agent-appeared-during-cleanup",
                    }),
                );
                continue;
            }
            let closed = self.command(&[&self.herdr_bin, "workspace", "close", &workspace.id]);
            if closed.returncode == 0 {
                closed_workspaces.insert(workspace.id.clone());
                self.logger.emit(
                    "info",
                    "workspace-closed",
                    &json!({"workspace": workspace.id}),
                );
            } else {
                self.logger.emit(
                    "warning",
                    "workspace-close-failed",
                    &json!({
                        "workspace": workspace.id,
                        "error": message(&closed),
                    }),
                );
            }
        }

        let pane_result = self.command(&[&self.herdr_bin, "pane", "list"]);
        if pane_result.returncode != 0 {
            self.logger.emit(
                "warning",
                "pane-refresh-failed",
                &json!({"error": message(&pane_result)}),
            );
            return;
        }
        let Ok(panes) = parse_panes(&pane_result.stdout) else {
            return;
        };
        let calling_pane = env::var("HERDR_PANE_ID").ok();
        for pane in panes {
            if closed_workspaces.contains(&pane.workspace_id)
                || calling_pane.as_deref() == Some(&pane.id)
            {
                continue;
            }
            let inside = [&pane.cwd, &pane.foreground_cwd]
                .into_iter()
                .flatten()
                .any(|candidate| is_under(&resolve_path(candidate), &resolve_path(path)));
            if !inside {
                continue;
            }
            if pane.has_agent {
                self.logger.emit(
                    "warning",
                    "pane-close-skipped",
                    &json!({"pane": pane.id, "reason": "agent-appeared-during-cleanup"}),
                );
                continue;
            }
            let closed = self.command(&[&self.herdr_bin, "pane", "close", &pane.id]);
            if closed.returncode == 0 {
                self.logger
                    .emit("info", "pane-closed", &json!({"pane": pane.id}));
            } else {
                self.logger.emit(
                    "warning",
                    "pane-close-failed",
                    &json!({"pane": pane.id, "error": message(&closed)}),
                );
            }
        }
    }
}

fn parse_worktrees(output: &str) -> Vec<Worktree> {
    if output.is_empty() {
        return Vec::new();
    }
    let records = if output.contains('\0') {
        output.split("\0\0").collect::<Vec<_>>()
    } else {
        output.split("\n\n").collect::<Vec<_>>()
    };
    records
        .iter()
        .enumerate()
        .filter_map(|(index, record)| {
            let normalized = record.replace('\0', "\n");
            let mut fields = BTreeMap::new();
            let mut flags = BTreeSet::new();
            for line in normalized.lines() {
                if let Some((key, value)) = line.split_once(' ') {
                    fields.insert(key, value);
                } else if !line.is_empty() {
                    flags.insert(line);
                }
            }
            Some(Worktree {
                path: PathBuf::from(*fields.get("worktree")?),
                branch_ref: fields.get("branch").map(ToString::to_string),
                head: fields.get("HEAD").map(ToString::to_string),
                is_main: index == 0,
                detached: flags.contains("detached"),
                prunable: fields.contains_key("prunable") || flags.contains("prunable"),
            })
        })
        .collect()
}

fn parse_workspaces(output: &str) -> Result<Vec<Workspace>, String> {
    let envelope: Value = serde_json::from_str(output).map_err(|error| error.to_string())?;
    let entries = envelope
        .pointer("/result/workspaces")
        .and_then(Value::as_array)
        .ok_or_else(|| "result.workspaces is not a list".to_owned())?;
    entries
        .iter()
        .map(|item| {
            let workspace_id = item
                .get("workspace_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "workspace_id is missing or invalid".to_owned())?;
            let worktree = item.get("worktree").and_then(Value::as_object);
            Ok(Workspace {
                id: workspace_id.into(),
                checkout_path: value_path(worktree.and_then(|value| value.get("checkout_path"))),
                repo_root: value_path(worktree.and_then(|value| value.get("repo_root"))),
                focused: truthy(item.get("focused")),
                has_agent: truthy(item.get("agent_status")),
            })
        })
        .collect()
}

fn parse_panes(output: &str) -> Result<Vec<Pane>, String> {
    let envelope: Value = serde_json::from_str(output).map_err(|error| error.to_string())?;
    let entries = envelope
        .pointer("/result/panes")
        .and_then(Value::as_array)
        .ok_or_else(|| "result.panes is not a list".to_owned())?;
    entries
        .iter()
        .map(|item| {
            Ok(Pane {
                id: item
                    .get("pane_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "pane_id is missing or invalid".to_owned())?
                    .into(),
                workspace_id: item
                    .get("workspace_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "workspace_id is missing or invalid".to_owned())?
                    .into(),
                cwd: value_path(item.get("cwd")),
                foreground_cwd: value_path(item.get("foreground_cwd")),
                focused: truthy(item.get("focused")),
                has_agent: truthy(item.get("agent")),
            })
        })
        .collect()
}

fn value_path(value: Option<&Value>) -> Option<PathBuf> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Number(value)) => value.as_f64().is_some_and(|value| value != 0.0),
        Some(Value::Array(value)) => !value.is_empty(),
        Some(Value::Object(value)) => !value.is_empty(),
    }
}

fn visit_repo_hints(value: &Value, key: Option<&str>, hints: &mut Vec<PathBuf>) {
    match value {
        Value::Object(object) => {
            for (child_key, child) in object {
                visit_repo_hints(child, Some(child_key), hints);
            }
        }
        Value::Array(array) => {
            for child in array {
                visit_repo_hints(child, key, hints);
            }
        }
        Value::String(path)
            if matches!(
                key,
                Some(
                    "repo_root"
                        | "checkout_path"
                        | "cwd"
                        | "foreground_cwd"
                        | "workspace_cwd"
                        | "focused_pane_cwd"
                )
            ) =>
        {
            hints.push(path.into());
        }
        _ => {}
    }
}

fn event_repo_hints(raw: &str) -> Vec<PathBuf> {
    let Ok(payload) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    let mut hints = Vec::new();
    visit_repo_hints(&payload, None, &mut hints);
    hints
}

fn processes_under(path: &Path, runner: &dyn Runner) -> Result<Vec<u32>, String> {
    let target = resolve_path(path);
    let proc = Path::new("/proc");
    if proc.is_dir() {
        let mut found = BTreeSet::new();
        let entries = fs::read_dir(proc).map_err(|error| error.to_string())?;
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|value| value.parse::<u32>().ok())
            else {
                continue;
            };
            if pid == std::process::id() {
                continue;
            }
            if let Ok(cwd) = fs::canonicalize(entry.path().join("cwd"))
                && is_under(&cwd, &target)
            {
                found.insert(pid);
            }
        }
        return Ok(found.into_iter().collect());
    }

    let result = runner.run(
        &[
            "lsof".into(),
            "-a".into(),
            "-d".into(),
            "cwd".into(),
            "-Fpn".into(),
            "+D".into(),
            target.to_string_lossy().into_owned(),
        ],
        Some(Duration::from_secs(10)),
        &[],
    );
    if result.returncode == 127 || result.returncode == 124 {
        return Err(message(&result));
    }
    let mut found = BTreeSet::new();
    let mut current_pid = None;
    for line in result.stdout.lines() {
        if let Some(value) = line.strip_prefix('p') {
            current_pid = value.parse::<u32>().ok();
        } else if let Some(value) = line.strip_prefix('n')
            && let Some(pid) = current_pid
            && pid != std::process::id()
            && is_under(&resolve_path(Path::new(value)), &target)
        {
            found.insert(pid);
        }
    }
    Ok(found.into_iter().collect())
}

fn resolve_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    if let Ok(resolved) = fs::canonicalize(&absolute) {
        return resolved;
    }

    // Resolve an existing ancestor too, so nonexistent descendants below a
    // symlinked path (for example /var -> /private/var on macOS) still compare
    // correctly with an existing checkout.
    let mut ancestor = absolute.as_path();
    let mut suffix: Vec<OsString> = Vec::new();
    loop {
        if let Ok(mut resolved) = fs::canonicalize(ancestor) {
            for component in suffix.iter().rev() {
                resolved.push(component);
            }
            return resolved;
        }
        let Some(name) = ancestor.file_name() else {
            return absolute;
        };
        suffix.push(name.to_os_string());
        let Some(parent) = ancestor.parent() else {
            return absolute;
        };
        ancestor = parent;
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    resolve_path(left) == resolve_path(right)
}

fn is_under(path: &Path, parent: &Path) -> bool {
    path.starts_with(parent)
}

fn message(result: &CommandResult) -> String {
    let value = if !result.stderr.trim().is_empty() {
        result.stderr.trim()
    } else if !result.stdout.trim().is_empty() {
        result.stdout.trim()
    } else {
        return format!("exit {}", result.returncode);
    };
    let start = value
        .char_indices()
        .rev()
        .take_while(|(index, _)| value.len() - index <= 1000)
        .last()
        .map_or(0, |(index, _)| index);
    value[start..].to_owned()
}

fn worktrunk_outcome(output: &str) -> Option<String> {
    output.lines().rev().find_map(|line| {
        serde_json::from_str::<Value>(line)
            .ok()?
            .get("branch_outcome")?
            .as_str()
            .map(ToString::to_string)
    })
}

fn home_dir() -> PathBuf {
    env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

fn default_config_dir() -> PathBuf {
    env::var_os("HERDR_PLUGIN_CONFIG_DIR").map_or_else(
        || home_dir().join(".config/herdr/plugins").join(PLUGIN_ID),
        PathBuf::from,
    )
}

fn default_state_dir() -> PathBuf {
    env::var_os("HERDR_PLUGIN_STATE_DIR").map_or_else(
        || {
            env::var_os("XDG_STATE_HOME")
                .map_or_else(|| home_dir().join(".local/state"), PathBuf::from)
                .join("herdr/plugins")
                .join(PLUGIN_ID)
        },
        PathBuf::from,
    )
}

fn unix_time() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

#[derive(Parser)]
#[command(about = "Event-driven, conservative cleanup for Herdr-managed Git worktrees")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Event(CommandArgs),
    Run(CommandArgs),
}

#[derive(Args)]
struct CommandArgs {
    #[arg(long)]
    trigger: Option<String>,
    #[arg(long, conflicts_with = "remove")]
    dry_run: bool,
    #[arg(long, conflicts_with = "dry_run")]
    remove: bool,
    #[arg(long)]
    force_fetch: bool,
}

fn main() {
    let cli = Cli::parse();
    let (is_event, args, default_trigger) = match cli.command {
        Commands::Event(args) => (true, args, "event"),
        Commands::Run(args) => (false, args, "run"),
    };
    let logger = Logger::new(args.trigger.as_deref().unwrap_or(default_trigger));
    let config = match Config::load(&default_config_dir()) {
        Ok(config) => config,
        Err(error) => {
            logger.emit("error", "config-invalid", &json!({"error": error}));
            std::process::exit(2);
        }
    };
    let remove = !args.dry_run && (args.remove || (is_event && config.auto_remove));
    let state_dir = default_state_dir();
    let lock = RunLock::acquire(state_dir.join("run.lock"));
    if !lock.acquired {
        logger.emit(
            "info",
            "skip",
            &json!({"reason": "cleanup-already-running"}),
        );
        return;
    }
    let runner = CommandRunner;
    let collector = WorktreeGc {
        runner: &runner,
        config,
        state_path: state_dir.join("state.json"),
        logger: &logger,
        herdr_bin: env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into()),
        wt_bin: "wt".into(),
        now: unix_time,
    };
    let status = collector.run(
        remove,
        args.force_fetch,
        &env::var("HERDR_PLUGIN_EVENT_JSON").unwrap_or_default(),
        &env::var("HERDR_PLUGIN_CONTEXT_JSON").unwrap_or_default(),
    );
    drop(lock);
    if status != 0 {
        std::process::exit(status);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::process::Command;
    use tempfile::TempDir;

    struct RecordingRunner {
        responses: RefCell<Vec<CommandResult>>,
        calls: RefCell<Vec<Vec<String>>>,
    }

    impl RecordingRunner {
        fn new(mut responses: Vec<CommandResult>) -> Self {
            responses.reverse();
            Self {
                responses: RefCell::new(responses),
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl Runner for RecordingRunner {
        fn run(
            &self,
            args: &[String],
            _timeout: Option<Duration>,
            _extra_env: &[(&str, &str)],
        ) -> CommandResult {
            self.calls.borrow_mut().push(args.to_vec());
            self.responses
                .borrow_mut()
                .pop()
                .unwrap_or_else(|| panic!("unexpected command: {args:?}"))
        }
    }

    fn result(returncode: i32, stdout: &str, stderr: &str) -> CommandResult {
        CommandResult {
            returncode,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    fn fixed_time() -> f64 {
        1000.0
    }

    fn collector<'a>(root: &Path, runner: &'a dyn Runner, logger: &'a Logger) -> WorktreeGc<'a> {
        WorktreeGc {
            runner,
            config: Config {
                check_processes: false,
                ..Config::default()
            },
            state_path: root.join("state.json"),
            logger,
            herdr_bin: "herdr-test".into(),
            wt_bin: "wt".into(),
            now: fixed_time,
        }
    }

    #[test]
    fn parses_worktree_porcelain_z() {
        let output = concat!(
            "worktree /repo\0HEAD aaa\0branch refs/heads/main\0\0",
            "worktree /repo-feature\0HEAD bbb\0branch refs/heads/feature/x\0\0",
            "worktree /repo-detached\0HEAD ccc\0detached\0\0"
        );
        let worktrees = parse_worktrees(output);
        assert_eq!(
            worktrees.iter().map(Worktree::branch).collect::<Vec<_>>(),
            vec![Some("main"), Some("feature/x"), None]
        );
        assert!(worktrees[0].is_main);
        assert!(!worktrees[1].is_main);
        assert!(worktrees[2].detached);
    }

    #[test]
    fn parses_herdr_resources() {
        let workspaces = parse_workspaces(
            r#"{"result":{"workspaces":[{"workspace_id":"w1","focused":true,"agent_status":"idle","worktree":{"checkout_path":"/repo","repo_root":"/repo"}}]}}"#,
        )
        .unwrap();
        let panes = parse_panes(
            r#"{"result":{"panes":[{"pane_id":"w1:p1","workspace_id":"w1","cwd":"/repo","foreground_cwd":"/repo/subdir","focused":true,"agent":"pi"}]}}"#,
        )
        .unwrap();
        assert!(workspaces[0].focused);
        assert!(workspaces[0].has_agent);
        assert_eq!(panes[0].foreground_cwd, Some(PathBuf::from("/repo/subdir")));
        assert!(panes[0].has_agent);
    }

    #[test]
    fn event_repository_hints_are_recursive() {
        let payload = r#"{"event":"workspace_closed","data":{"workspace":{"worktree":{"repo_root":"/repo","checkout_path":"/repo-feature"}}}}"#;
        assert_eq!(
            event_repo_hints(payload),
            vec![PathBuf::from("/repo"), PathBuf::from("/repo-feature")]
        );
    }

    #[test]
    fn config_defaults_to_preview_and_validates_types() {
        let temp = TempDir::new().unwrap();
        assert_eq!(Config::load(temp.path()).unwrap(), Config::default());
        fs::write(
            temp.path().join("config.toml"),
            "auto_remove = true\ndebounce_seconds = 12\nfetch_timeout_seconds = 4\n",
        )
        .unwrap();
        let config = Config::load(temp.path()).unwrap();
        assert!(config.auto_remove);
        assert_eq!(config.debounce_seconds, 12);
        fs::write(
            temp.path().join("config.toml"),
            "debounce_seconds = \"often\"\n",
        )
        .unwrap();
        assert!(Config::load(temp.path()).is_err());
    }

    #[test]
    fn state_round_trips_and_recovers_from_invalid_json() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("state.json");
        let mut state = State::default();
        state.last_fetch.insert("/repo/.git".into(), 42.5);
        state.save(&path).unwrap();
        assert_eq!(State::load(&path), state);

        fs::write(&path, "not json").unwrap();
        assert_eq!(State::load(&path), State::default());
    }

    #[test]
    fn main_focused_and_agent_worktrees_are_skipped() {
        let temp = TempDir::new().unwrap();
        let runner = RecordingRunner::new(Vec::new());
        let logger = Logger::new("test");
        let gc = collector(temp.path(), &runner, &logger);
        let main = Worktree {
            path: temp.path().into(),
            branch_ref: Some("refs/heads/main".into()),
            head: Some("aaa".into()),
            is_main: true,
            detached: false,
            prunable: false,
        };
        let linked_path = temp.path().join("linked");
        fs::create_dir(&linked_path).unwrap();
        let linked = Worktree {
            path: linked_path.clone(),
            branch_ref: Some("refs/heads/feature".into()),
            head: Some("bbb".into()),
            is_main: false,
            detached: false,
            prunable: false,
        };
        assert_eq!(
            gc.skip_reason(&main, &[], &[], &[]).as_deref(),
            Some("main-checkout")
        );
        let focused = Workspace {
            id: "w1".into(),
            checkout_path: Some(linked_path.clone()),
            repo_root: Some(temp.path().into()),
            focused: true,
            has_agent: false,
        };
        assert_eq!(
            gc.skip_reason(&linked, &[focused], &[], &[]).as_deref(),
            Some("current-worktree")
        );
        let agent = Pane {
            id: "w1:p1".into(),
            workspace_id: "w1".into(),
            cwd: Some(linked_path.clone()),
            foreground_cwd: Some(linked_path.clone()),
            focused: false,
            has_agent: true,
        };
        assert_eq!(
            gc.skip_reason(&linked, &[], &[agent], &[]).as_deref(),
            Some("active-agent")
        );
        assert_eq!(
            gc.skip_reason(&linked, &[], &[], &[linked_path.join("subdirectory")])
                .as_deref(),
            Some("current-worktree")
        );
    }

    #[test]
    fn removal_uses_worktrunk_without_force_flags() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("linked");
        fs::create_dir(&path).unwrap();
        let runner = RecordingRunner::new(vec![
            result(0, "{\"branch_outcome\":\"deleted\"}\n", ""),
            result(0, "{\"result\":{\"workspaces\":[]}}", ""),
            result(0, "{\"result\":{\"panes\":[]}}", ""),
        ]);
        let logger = Logger::new("test");
        let gc = collector(temp.path(), &runner, &logger);
        let worktree = Worktree {
            path,
            branch_ref: Some("refs/heads/feature".into()),
            head: Some("abc".into()),
            is_main: false,
            detached: false,
            prunable: false,
        };
        assert!(gc.remove_worktree(temp.path(), &worktree, "refs/remotes/origin/feature"));
        let calls = runner.calls.borrow();
        let command = &calls[0];
        assert_eq!(command[0], "wt");
        assert!(command.contains(&"--foreground".into()));
        assert!(command.contains(&"--yes".into()));
        for forbidden in ["--force", "--force-delete", "-D", "--reap"] {
            assert!(!command.contains(&forbidden.into()));
        }
    }

    #[test]
    fn fetch_failure_does_not_update_debounce_state() {
        let temp = TempDir::new().unwrap();
        let runner = RecordingRunner::new(vec![result(1, "", "offline")]);
        let logger = Logger::new("test");
        let gc = collector(temp.path(), &runner, &logger);
        let mut state = State::default();
        assert!(!gc.fetch(temp.path(), "/repo/.git", &mut state, true));
        assert!(state.last_fetch.is_empty());
    }

    #[test]
    fn run_lock_rejects_overlap() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("lock");
        let first = RunLock::acquire(path.clone());
        assert!(first.acquired);
        let second = RunLock::acquire(path);
        assert!(!second.acquired);
    }

    #[test]
    fn missing_upstream_is_detected_from_git_refs() {
        let temp = TempDir::new().unwrap();
        let remote = temp.path().join("remote.git");
        let repo = temp.path().join("repo");
        let linked = temp.path().join("linked");
        git(temp.path(), &["init", "--bare", remote.to_str().unwrap()]);
        git(temp.path(), &["init", "-b", "main", repo.to_str().unwrap()]);
        git(&repo, &["config", "user.name", "Test"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        fs::write(repo.join("file"), "main\n").unwrap();
        git(&repo, &["add", "file"]);
        git(&repo, &["commit", "-m", "initial"]);
        git(
            &repo,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&repo, &["push", "-u", "origin", "main"]);
        git(&repo, &["branch", "feature"]);
        git(&repo, &["push", "-u", "origin", "feature"]);
        git(
            &repo,
            &["worktree", "add", linked.to_str().unwrap(), "feature"],
        );
        git(&repo, &["update-ref", "-d", "refs/remotes/origin/feature"]);
        let head = git(&repo, &["rev-parse", "feature"]);
        let runner = CommandRunner;
        let logger = Logger::new("test");
        let gc = collector(temp.path(), &runner, &logger);
        let worktree = Worktree {
            path: linked,
            branch_ref: Some("refs/heads/feature".into()),
            head: Some(head.trim().into()),
            is_main: false,
            detached: false,
            prunable: false,
        };
        assert_eq!(
            gc.gone_upstream(&repo, &worktree).as_deref(),
            Some("refs/remotes/origin/feature")
        );
    }

    fn git(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
}
