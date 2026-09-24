//! `forgeline`: run swarms of coding agents from the terminal.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use forgeline_agents::{AgentRuntime, ClaudeCode, MockAgent};
use forgeline_config::{AgentConfig, CONFIG_FILE, Config, Plan, TEMPLATE};
use forgeline_core::{ApprovalId, Decision, RunId, RunState, WorkSource};
use forgeline_engine::{Engine, NewRun};
use forgeline_store::EventStore;
use forgeline_workspace::GitRepo;

mod ui;

/// `println!` that exits quietly when stdout is closed (e.g. piped into
/// `head`) instead of panicking.
macro_rules! outln {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        if let Err(e) = writeln!(std::io::stdout().lock(), $($arg)*) {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                std::process::exit(0);
            }
        }
    }};
}

#[derive(Parser)]
#[command(
    name = "forgeline",
    version,
    about = "An open-source software factory: orchestrate swarms of coding agents from work item to verified, integrated code."
)]
struct Cli {
    /// Repository to operate on (defaults to the current directory).
    #[arg(long, global = true, value_name = "PATH")]
    repo: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Set up Forgeline in this repository (forgeline.toml, .gitignore).
    Init,
    /// Start a run and follow it until it finishes.
    Run {
        /// What to build: a one-line title, optionally followed by details.
        request: String,
        /// Read the full request from a file (the positional argument becomes the title).
        #[arg(long, value_name = "FILE")]
        request_file: Option<PathBuf>,
        /// A plan (TOML or JSON) splitting the work into tasks. Without one,
        /// the whole request is a single task.
        #[arg(long, value_name = "FILE")]
        plan: Option<PathBuf>,
        /// Branch to build on (defaults to the configured base or the current branch).
        #[arg(long)]
        base: Option<String>,
        /// Use this agent runtime for every role (`claude-code`, `mock`).
        #[arg(long)]
        agent: Option<String>,
        /// Model for every role (runtime-specific name).
        #[arg(long)]
        model: Option<String>,
        /// Stop the run when agents have spent this many US dollars.
        #[arg(long, value_name = "USD")]
        max_cost: Option<f64>,
        /// Agents running at once.
        #[arg(long)]
        parallel: Option<usize>,
        /// Show what agents are doing (messages, tool calls).
        #[arg(short, long)]
        verbose: bool,
    },
    /// Continue a run (after a crash, Ctrl-C, or a restart).
    Resume {
        run: String,
        #[arg(long)]
        agent: Option<String>,
        #[arg(short, long)]
        verbose: bool,
    },
    /// List runs.
    Runs,
    /// Show a run: tasks, attempts, checks, spend.
    Show { run: String },
    /// Print a run's events.
    Events {
        run: String,
        /// Keep printing new events as they happen.
        #[arg(short, long)]
        follow: bool,
        /// One JSON object per line.
        #[arg(long)]
        json: bool,
    },
    /// List decisions waiting for a human.
    Approvals,
    /// Approve (or deny) a pending request.
    Approve {
        approval: String,
        #[arg(long)]
        deny: bool,
        #[arg(long)]
        comment: Option<String>,
    },
    /// Check that everything Forgeline needs is installed and configured.
    Doctor,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("FORGELINE_LOG")
                .unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    if let Err(e) = dispatch(cli).await {
        eprintln!("{} {e:#}", ui::red("error:"));
        std::process::exit(1);
    }
}

async fn dispatch(cli: Cli) -> Result<()> {
    let start = cli.repo.clone().unwrap_or_else(|| PathBuf::from("."));
    match cli.command {
        Command::Init => init(&start).await,
        Command::Run {
            request,
            request_file,
            plan,
            base,
            agent,
            model,
            max_cost,
            parallel,
            verbose,
        } => {
            let project = Project::open(&start, base.as_deref()).await?;
            let mut config = project.config.clone();
            override_agents(&mut config, agent.as_deref(), model.as_deref());
            if let Some(parallel) = parallel {
                config.limits.max_parallel_agents = parallel;
            }
            let engine = project.engine(config)?;
            let (title, body) = match &request_file {
                Some(path) => (
                    request.clone(),
                    std::fs::read_to_string(path)
                        .with_context(|| format!("reading {}", path.display()))?,
                ),
                None => (
                    request.lines().next().unwrap_or_default().to_owned(),
                    request.clone(),
                ),
            };
            let plan = match &plan {
                Some(path) => Plan::parse(
                    &std::fs::read_to_string(path)
                        .with_context(|| format!("reading {}", path.display()))?,
                )
                .with_context(|| format!("in {}", path.display()))?,
                None => Plan::single(&title, &body),
            };
            let run_id = engine
                .create_run(NewRun {
                    title,
                    request: body,
                    source: WorkSource::Manual,
                    base_ref: base,
                    plan,
                    max_cost_usd: max_cost,
                    max_tokens: None,
                })
                .await?;
            outln!("{} {}", ui::bold("started"), run_id);
            follow_drive(&engine, run_id, verbose).await
        }
        Command::Resume {
            run,
            agent,
            verbose,
        } => {
            let project = Project::open(&start, None).await?;
            let run_id = project.resolve_run(&run).await?;
            let mut config = project.config.clone();
            override_agents(&mut config, agent.as_deref(), None);
            let engine = project.engine(config)?;
            follow_drive(&engine, run_id, verbose).await
        }
        Command::Runs => {
            let project = Project::open(&start, None).await?;
            let runs = project.store.list_runs().await?;
            if runs.is_empty() {
                outln!("No runs yet. Start one with `forgeline run \"…\"`.");
            }
            for run in runs {
                outln!(
                    "{}  {:<22}  {}/{} tasks  {}  {}",
                    ui::dim(&forgeline_engine::short_id(run.run_id)),
                    ui::run_status(run.status),
                    run.tasks_done,
                    run.tasks_total,
                    ui::dim(&ui::usage(&run.usage)),
                    run.title
                );
            }
            Ok(())
        }
        Command::Show { run } => {
            let project = Project::open(&start, None).await?;
            let run_id = project.resolve_run(&run).await?;
            let state = project.load(run_id).await?;
            print_run(&state);
            Ok(())
        }
        Command::Events { run, follow, json } => {
            let project = Project::open(&start, None).await?;
            let run_id = project.resolve_run(&run).await?;
            print_events(&project.store, run_id, follow, json).await
        }
        Command::Approvals => {
            let project = Project::open(&start, None).await?;
            let mut any = false;
            for summary in project.store.list_runs().await? {
                if summary.pending_approvals == 0 {
                    continue;
                }
                let state = project.load(summary.run_id).await?;
                for approval in state.pending_approvals() {
                    any = true;
                    outln!(
                        "{}  {}  {}\n{}\n",
                        ui::bold(&approval.id.to_string()),
                        ui::dim(&format!("{:?}", approval.kind)),
                        approval.title,
                        indent(&approval.details)
                    );
                }
            }
            if !any {
                outln!("Nothing is waiting for you.");
            }
            Ok(())
        }
        Command::Approve {
            approval,
            deny,
            comment,
        } => {
            let project = Project::open(&start, None).await?;
            let approval_id: ApprovalId = approval.parse()?;
            let run_id = project.find_approval(approval_id).await?;
            let decision = if deny {
                Decision::Rejected
            } else {
                Decision::Approved
            };
            let by = std::env::var("USER").unwrap_or_else(|_| "cli".into());
            let engine = project.engine(project.config.clone())?;
            engine
                .resolve_approval(run_id, approval_id, decision, &by, comment)
                .await?;
            outln!("{decision:?}.");
            Ok(())
        }
        Command::Doctor => doctor(&start).await,
    }
}

/// Everything a command needs about the repository.
struct Project {
    repo: GitRepo,
    config: Config,
    store: EventStore,
    state_dir: PathBuf,
}

impl Project {
    async fn open(start: &Path, base: Option<&str>) -> Result<Self> {
        let repo = GitRepo::open(start)
            .await
            .context("Forgeline works inside a git repository")?;
        let config = load_config(&repo, base).await?;
        let state_dir = repo.root().join(&config.project.state_dir);
        std::fs::create_dir_all(&state_dir)
            .with_context(|| format!("creating {}", state_dir.display()))?;
        let store = EventStore::open(state_dir.join("forgeline.sqlite"))?;
        Ok(Self {
            repo,
            config,
            store,
            state_dir,
        })
    }

    fn engine(&self, config: Config) -> Result<Engine> {
        let runtimes: [Arc<dyn AgentRuntime>; 2] =
            [Arc::new(ClaudeCode::new()), Arc::new(MockAgent::demo())];
        let mut engine = Engine::new(
            self.store.clone(),
            self.repo.clone(),
            config,
            self.state_dir.clone(),
        );
        for runtime in runtimes {
            engine = engine.with_runtime(runtime);
        }
        Ok(engine)
    }

    async fn load(&self, run_id: RunId) -> Result<RunState> {
        self.store
            .run(run_id)
            .await?
            .with_context(|| format!("run {run_id} not found"))
    }

    /// Accepts a full run id or the short id shown by `forgeline runs`.
    async fn resolve_run(&self, input: &str) -> Result<RunId> {
        if let Ok(id) = input.parse::<RunId>() {
            return Ok(id);
        }
        let matches: Vec<RunId> = self
            .store
            .list_runs()
            .await?
            .into_iter()
            .map(|r| r.run_id)
            .filter(|id| forgeline_engine::short_id(*id).starts_with(input))
            .collect();
        match matches.as_slice() {
            [id] => Ok(*id),
            [] => bail!("no run matches `{input}` (see `forgeline runs`)"),
            _ => bail!("`{input}` matches several runs; use more characters"),
        }
    }

    async fn find_approval(&self, approval_id: ApprovalId) -> Result<RunId> {
        for summary in self.store.list_runs().await? {
            let state = self.load(summary.run_id).await?;
            if state.approvals.contains_key(&approval_id) {
                return Ok(summary.run_id);
            }
        }
        bail!("no pending approval {approval_id}")
    }
}

/// Reads `forgeline.toml` from the base branch, so agents cannot change their
/// own checks. Falls back to the working tree when it is not committed yet.
async fn load_config(repo: &GitRepo, base: Option<&str>) -> Result<Config> {
    let base = match base {
        Some(base) => base.to_owned(),
        None => repo
            .current_branch()
            .await?
            .unwrap_or_else(|| "HEAD".into()),
    };
    if let Some(text) = repo.show_file(&base, CONFIG_FILE).await? {
        return Config::parse(&text).with_context(|| format!("in {CONFIG_FILE} on `{base}`"));
    }
    let local = repo.root().join(CONFIG_FILE);
    if local.exists() {
        eprintln!(
            "{} {CONFIG_FILE} is not committed on `{base}`; using the working-tree copy. Commit it so \
             runs read checks and permissions from the base branch.",
            ui::yellow("note:")
        );
        let text = std::fs::read_to_string(&local)?;
        return Config::parse(&text).with_context(|| format!("in {}", local.display()));
    }
    Ok(Config::default())
}

fn override_agents(config: &mut Config, runtime: Option<&str>, model: Option<&str>) {
    if runtime.is_none() && model.is_none() {
        return;
    }
    if config.agents.is_empty() {
        config
            .agents
            .insert("default".into(), AgentConfig::default());
    }
    for agent in config.agents.values_mut() {
        if let Some(runtime) = runtime {
            agent.runtime = runtime.to_owned();
        }
        if let Some(model) = model {
            agent.model = Some(model.to_owned());
        }
    }
}

/// Drives a run while printing its events; Ctrl-C stops cleanly.
async fn follow_drive(engine: &Engine, run_id: RunId, verbose: bool) -> Result<()> {
    let store = engine.store().clone();
    let mut events = store.subscribe();
    let printer = tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) if event.run_id == run_id => {
                    let Ok(Some(state)) = store.run(run_id).await else {
                        continue;
                    };
                    if let Some(line) = ui::describe(&event, &state, verbose) {
                        outln!("{line}");
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    outln!("{}", ui::dim(&format!("… {n} events not shown")));
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let result = tokio::select! {
        result = engine.drive(run_id) => result,
        _ = tokio::signal::ctrl_c() => {
            printer.abort();
            outln!(
                "\n{} agents stopped. Continue with `forgeline resume {}`.",
                ui::yellow("interrupted:"),
                forgeline_engine::short_id(run_id)
            );
            return Ok(());
        }
    };
    // Let the printer catch up with the last events.
    tokio::time::sleep(Duration::from_millis(200)).await;
    printer.abort();
    let state = result?;
    outln!();
    print_run(&state);
    Ok(())
}

fn print_run(state: &RunState) {
    outln!(
        "{} {}  {}",
        ui::bold(&state.spec.title),
        ui::dim(&state.id.to_string()),
        ui::run_status(state.status)
    );
    if let Some(reason) = &state.status_reason {
        outln!("  {}", reason);
    }
    outln!(
        "  base {}  →  integration branch {}",
        state.spec.base_ref,
        ui::bold(&Engine::integration_branch(state.id))
    );
    outln!("  spent {}", ui::usage(&state.total_usage()));
    outln!();
    let key_width = state
        .tasks
        .values()
        .map(|t| t.spec.key.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let order = state.graph().topo_order().unwrap_or_default();
    for task_id in order {
        let task = &state.tasks[&task_id];
        let deps = if task.spec.depends_on.is_empty() {
            String::new()
        } else {
            ui::dim(&format!(" ← {}", task.spec.depends_on.join(", ")))
        };
        outln!(
            "  {:<key_width$}  {:<9} {}{}",
            task.spec.key,
            ui::task_status(task.status),
            task.spec.title,
            deps
        );
        for attempt_id in &task.attempts {
            let attempt = &state.attempts[attempt_id];
            let checks: Vec<String> = attempt
                .checks
                .iter()
                .map(|c| {
                    if c.passed {
                        ui::green(&c.name)
                    } else {
                        ui::red(&c.name)
                    }
                })
                .collect();
            let outcome = match &attempt.outcome {
                None => ui::cyan("running"),
                Some(forgeline_core::AttemptOutcome::Succeeded { .. }) => ui::green("verified"),
                Some(forgeline_core::AttemptOutcome::Failed { kind, .. }) => {
                    ui::red(&format!("failed ({kind:?})"))
                }
                Some(forgeline_core::AttemptOutcome::Cancelled) => ui::dim("cancelled"),
            };
            outln!(
                "  {:<key_width$}  #{} {}  {}  {}  {}",
                "",
                attempt.number,
                outcome,
                ui::dim(&attempt.branch),
                ui::dim(&ui::usage(&attempt.usage)),
                checks.join(" ")
            );
        }
    }
    let pending: Vec<_> = state.pending_approvals().collect();
    if !pending.is_empty() {
        outln!();
        for approval in pending {
            outln!(
                "  {} {}  →  forgeline approve {}",
                ui::yellow("waiting:"),
                approval.title,
                approval.id
            );
        }
    }
    if state.status == forgeline_core::RunStatus::Succeeded {
        let branch = Engine::integration_branch(state.id);
        outln!();
        outln!(
            "  Review: git log --oneline {}..{branch}",
            state.spec.base_ref
        );
        outln!("  Merge:  git merge {branch}");
    }
}

async fn print_events(store: &EventStore, run_id: RunId, follow: bool, json: bool) -> Result<()> {
    let mut after = 0;
    loop {
        let events = store.run_events(run_id, after, 1_000).await?;
        if let Some(last) = events.last() {
            after = last.seq;
        }
        let state = store.run(run_id).await?;
        for event in &events {
            if json {
                outln!("{}", serde_json::to_string(event)?);
            } else if let Some(state) = &state
                && let Some(line) = ui::describe(event, state, true)
            {
                outln!("{} {line}", ui::dim(&format!("{:>6}", event.seq)));
            }
        }
        let finished = state.is_some_and(|s| s.status.is_terminal());
        if !follow || (finished && events.is_empty()) {
            return Ok(());
        }
        if events.is_empty() {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

async fn init(start: &Path) -> Result<()> {
    let repo = GitRepo::open(start)
        .await
        .context("run `git init` first: Forgeline works inside a git repository")?;
    let root = repo.root();
    let config_path = root.join(CONFIG_FILE);
    if config_path.exists() {
        outln!("{CONFIG_FILE} already exists; leaving it alone.");
    } else {
        std::fs::write(&config_path, TEMPLATE)?;
        outln!("{} {CONFIG_FILE}", ui::green("created"));
    }
    let gitignore = root.join(".gitignore");
    let existing = std::fs::read_to_string(&gitignore).unwrap_or_default();
    if !existing.lines().any(|l| l.trim() == ".forgeline/") {
        let mut text = existing;
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("\n# Forgeline state (database, agent worktrees)\n.forgeline/\n");
        std::fs::write(&gitignore, text)?;
        outln!("{} .forgeline/ to .gitignore", ui::green("added"));
    }
    outln!(
        "\nNext:\n  1. Edit {CONFIG_FILE}: add your [[checks]] (build, tests) and pick an agent.\n  \
         2. Commit it: git add {CONFIG_FILE} .gitignore && git commit -m \"Add Forgeline\"\n  \
         3. Try it without spending tokens: forgeline run \"Say hello\" --agent mock\n  \
         4. Then for real: forgeline run \"<what to build>\" [--plan plan.toml]"
    );
    Ok(())
}

async fn doctor(start: &Path) -> Result<()> {
    let ok = |label: &str, detail: &str| outln!("{} {label} {}", ui::green("✓"), ui::dim(detail));
    let warn = |label: &str, detail: &str| outln!("{} {label} {detail}", ui::yellow("!"));
    let bad = |label: &str, detail: &str| outln!("{} {label} {detail}", ui::red("✗"));

    let repo = match GitRepo::open(start).await {
        Ok(repo) => {
            ok("git repository", &repo.root().display().to_string());
            repo
        }
        Err(e) => {
            bad("git repository", &e.to_string());
            return Ok(());
        }
    };
    let config = match load_config(&repo, None).await {
        Ok(config) => {
            ok(CONFIG_FILE, "valid");
            config
        }
        Err(e) => {
            bad(CONFIG_FILE, &format!("{e:#}"));
            Config::default()
        }
    };
    if config.checks.is_empty() {
        warn(
            "checks",
            "none configured: work is integrated without verification. Add [[checks]] to forgeline.toml.",
        );
    } else {
        ok("checks", &format!("{} configured", config.checks.len()));
    }
    let ignored = std::fs::read_to_string(repo.root().join(".gitignore"))
        .unwrap_or_default()
        .lines()
        .any(|l| l.trim().trim_end_matches('/') == config.project.state_dir.trim_end_matches('/'));
    if ignored {
        ok("state directory ignored by git", &config.project.state_dir);
    } else {
        warn(
            "state directory",
            &format!(
                "add `{}/` to .gitignore (forgeline init does this)",
                config.project.state_dir
            ),
        );
    }
    let mut runtimes: Vec<String> = config.agents.values().map(|a| a.runtime.clone()).collect();
    if runtimes.is_empty() {
        runtimes.push(AgentConfig::default().runtime);
    }
    runtimes.sort();
    runtimes.dedup();
    for name in runtimes {
        let report = match name.as_str() {
            "claude-code" => ClaudeCode::new().probe().await,
            "mock" => MockAgent::demo().probe().await,
            other => {
                bad(&format!("agent `{other}`"), "unknown runtime");
                continue;
            }
        };
        if report.available {
            ok(
                &format!("agent `{name}`"),
                report.version.as_deref().unwrap_or(""),
            );
        } else {
            bad(
                &format!("agent `{name}`"),
                report.detail.as_deref().unwrap_or("not available"),
            );
        }
    }
    Ok(())
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}
