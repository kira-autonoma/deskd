//! CLI definitions — clap structs and argument parsing.

use clap::{Parser, Subcommand};

pub const DEFAULT_SOCKET: &str = "/tmp/deskd.sock";

pub fn version_string() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| {
        let ver = option_env!("DESKD_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"));
        let hash = env!("GIT_HASH");
        if hash.is_empty() {
            ver.to_string()
        } else {
            format!("{ver} ({hash})")
        }
    })
}

#[derive(Parser)]
#[command(name = "deskd", about = "Agent orchestration runtime", version = version_string())]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Start the bus and launch all persistent agents from workspace config.
    Serve {
        /// Path to workspace.yaml.
        #[arg(long)]
        config: String,
    },
    /// Run as MCP server for a specific agent (called by claude --mcp-server).
    /// Provides send_message and add_persistent_agent tools.
    Mcp {
        /// Agent name (must match an agent registered in deskd state).
        #[arg(long)]
        agent: String,
    },
    /// Manage agents.
    Agent {
        #[command(subcommand)]
        action: AgentAction,
    },
    /// Show live status of all agents defined in workspace config.
    Status {
        /// Path to workspace.yaml. Auto-detected from running serve if omitted.
        #[arg(long)]
        config: Option<String>,
    },
    /// Kill the running `deskd serve` process and restart it with the same config.
    Restart {
        /// Path to workspace.yaml. Required if the running process cannot be auto-detected.
        #[arg(long)]
        config: Option<String>,
    },
    /// Run an executable skill graph from a YAML file.
    Graph {
        #[command(subcommand)]
        action: GraphAction,
    },
    /// Download the latest deskd release binary, replace the current installation,
    /// then restart deskd serve if it is running.
    Upgrade {
        /// Install directory. Defaults to the directory of the current binary,
        /// falling back to ~/.local/bin.
        #[arg(long)]
        install_dir: Option<String>,
    },
    /// State machine: manage models and instances.
    Sm {
        /// Path to deskd.yaml with model definitions. Auto-detected from running serve if omitted.
        #[arg(long, env = "DESKD_AGENT_CONFIG")]
        config: Option<String>,
        #[command(subcommand)]
        action: SmAction,
    },
    /// Manage cron schedules in the agent's deskd.yaml.
    ///
    /// List, add, or remove scheduled actions without editing YAML manually.
    /// Operates on `./deskd.yaml` by default (override with --config).
    ///
    /// Examples:
    ///   deskd schedule list
    ///   deskd schedule add --cron "0 */5 * * * *" --action github_poll --target "agent:dev"
    ///   deskd schedule rm 0
    Schedule {
        #[command(subcommand)]
        action: ScheduleSubcommand,
        /// Path to deskd.yaml. Auto-detected from running serve if omitted.
        #[arg(long, global = true)]
        config: Option<String>,
    },
    /// Show bus diagnostics and connected clients.
    Bus {
        #[command(subcommand)]
        action: BusAction,
    },
    /// Manage the pull-based task queue.
    Task {
        #[command(subcommand)]
        action: TaskAction,
    },
    /// A2A protocol: Agent Card generation and (future) HTTP server.
    A2a {
        #[command(subcommand)]
        action: A2aAction,
    },
    /// Show aggregate token usage and cost across all agents.
    ///
    /// Examples:
    ///   deskd usage                    # last 7 days, all agents
    ///   deskd usage --period today     # today only
    ///   deskd usage --period 30d       # last 30 days
    ///   deskd usage --agent dev        # filter to one agent
    ///   deskd usage --format json      # machine-readable output
    Usage {
        /// Time period: "today", "24h", "7d" (default), "30d", "all".
        #[arg(long, default_value = "7d")]
        period: String,
        /// Filter to a specific agent.
        #[arg(long)]
        agent: Option<String>,
        /// Output format: "table" (default) or "json".
        #[arg(long, default_value = "table")]
        format: String,
    },
    /// Launch the Ink/React TUI (connects to running deskd serve).
    ///
    /// Spawns `npx tsx tui/src/index.tsx` (or `bun run`) and forwards
    /// stdin/stdout/stderr. Requires Node.js/bun and `npm install` in tui/.
    ///
    /// Examples:
    ///   deskd tui
    ///   deskd tui --socket /home/kira/.deskd/bus.sock
    Tui {
        /// Bus socket path. Auto-detected from running serve if omitted.
        #[arg(long)]
        socket: Option<String>,
    },
    /// Schedule a one-shot reminder for an agent.
    ///
    /// Writes a RemindDef JSON to ~/.deskd/reminders/<uuid>.json.
    /// The reminder runner (part of `deskd serve`) will fire it when due.
    ///
    /// Examples:
    ///   deskd remind kira --in 30m "Check PR status"
    ///   deskd remind kira --in 2h30m "Stand-up time"
    ///   deskd remind kira --at 2026-03-27T15:00:00Z "Deploy window opens"
    ///   deskd remind kira --in 1h --target queue:reviews "Review queue check"
    Remind {
        /// Agent name. Used as bus target `agent:<name>` unless --target is given.
        name: String,
        /// Duration from now (e.g. 30m, 1h, 2h30m, 90s). Mutually exclusive with --at.
        #[arg(long, conflicts_with = "at")]
        r#in: Option<String>,
        /// Absolute ISO 8601 timestamp. Mutually exclusive with --in.
        #[arg(long, conflicts_with = "in")]
        at: Option<String>,
        /// Override bus target (default: agent:<name>).
        #[arg(long)]
        target: Option<String>,
        /// Message payload to deliver.
        message: String,
    },
}

#[derive(Subcommand)]
pub enum AgentAction {
    /// Register a new agent (saves state file, does not start worker).
    Create {
        name: String,
        #[arg(long)]
        prompt: Option<String>,
        #[arg(long, default_value = "claude-sonnet-4-6")]
        model: String,
        #[arg(long)]
        workdir: Option<String>,
        #[arg(long, default_value = "100")]
        max_turns: u32,
        #[arg(long)]
        unix_user: Option<String>,
        #[arg(long, default_value = "50.0")]
        budget_usd: f64,
        #[arg(long, num_args = 1.., value_delimiter = ' ')]
        command: Vec<String>,
    },
    /// Send a task to an agent (via bus if running, or directly).
    Send {
        name: String,
        message: String,
        #[arg(long)]
        max_turns: Option<u32>,
        /// Bus socket path. When omitted, resolved from agent state file
        /// (~/.deskd/agents/<name>.yaml → {work_dir}/.deskd/bus.sock).
        #[arg(long)]
        socket: Option<String>,
    },
    /// Start the worker loop for an agent (connect to bus, process tasks).
    Run {
        name: String,
        #[arg(long, default_value = DEFAULT_SOCKET)]
        socket: String,
        /// Custom subscriptions (overrides defaults). Can be repeated.
        #[arg(long)]
        subscribe: Vec<String>,
    },
    /// List registered agents with live status.
    List {
        #[arg(long, default_value = DEFAULT_SOCKET)]
        socket: String,
    },
    /// Show detailed stats for an agent.
    Stats { name: String },
    /// Read buffered task results from an agent's inbox.
    Read {
        name: String,
        /// Remove messages after reading.
        #[arg(long, default_value = "false")]
        clear: bool,
        /// Keep watching for new messages after printing existing ones.
        #[arg(long, default_value = "false")]
        follow: bool,
    },
    /// Show recent completed tasks for an agent (from inbox files).
    Tasks {
        /// Agent name, or "all" to show tasks for all agents.
        name: String,
        /// Show last N tasks (default 20).
        #[arg(long, default_value = "20")]
        limit: usize,
    },
    /// Show task history for an agent (from structured task log).
    Logs {
        /// Agent name.
        name: String,
        /// Show last N tasks (default 20).
        #[arg(long, default_value = "20")]
        limit: usize,
        /// Filter by source (e.g. telegram, github_poll, schedule).
        #[arg(long)]
        source: Option<String>,
        /// Show tasks from the last duration (e.g. 1h, 24h, 7d).
        #[arg(long)]
        since: Option<String>,
        /// Output raw JSONL instead of formatted table.
        #[arg(long, default_value = "false")]
        json: bool,
        /// Show cost summary instead of task list.
        #[arg(long, default_value = "false")]
        cost: bool,
        /// Show token usage grouped by GitHub PR.
        #[arg(long, default_value = "false")]
        by_pr: bool,
    },
    /// Show agent status: all agents (no name) or detail for one agent.
    Status {
        /// Agent name. Omit to show all agents.
        name: Option<String>,
    },
    /// Show stderr output from the agent's process.
    Stderr {
        /// Agent name.
        name: String,
        /// Show last N lines (default 50).
        #[arg(long, default_value = "50")]
        tail: usize,
        /// Keep watching for new output.
        #[arg(long, default_value = "false")]
        follow: bool,
    },
    /// Show parsed stream-json output (tool calls, responses, errors).
    Stream {
        /// Agent name.
        name: String,
        /// Show last N events (default 50).
        #[arg(long, default_value = "50")]
        tail: usize,
        /// Keep watching for new output.
        #[arg(long, default_value = "false")]
        follow: bool,
        /// Show raw JSONL instead of parsed output.
        #[arg(long, default_value = "false")]
        raw: bool,
    },
    /// Remove an agent (state file + log).
    Rm { name: String },
    /// Spawn an ephemeral sub-agent, run a task, print result, clean up.
    Spawn {
        name: String,
        task: String,
        /// Bus socket (defaults to $DESKD_BUS_SOCKET).
        #[arg(long)]
        socket: Option<String>,
        #[arg(long)]
        work_dir: Option<String>,
        #[arg(long, default_value = "claude-sonnet-4-6")]
        model: String,
        #[arg(long, default_value = "50")]
        max_turns: u32,
    },
    /// Update an agent's settings in workspace.yaml.
    ///
    /// Currently supports switching the named container profile referenced
    /// by the agent. Edits the workspace.yaml file in place. Run
    /// `deskd restart` afterwards for the change to take effect.
    ///
    /// Examples:
    ///   deskd agent set uagent --container work
    ///   deskd agent set uagent --container gcp --config /etc/deskd/workspace.yaml
    Set {
        /// Agent name as defined in workspace.yaml.
        name: String,
        /// Switch the agent to a named container profile from the
        /// top-level `containers:` map.
        #[arg(long)]
        container: Option<String>,
        /// Path to workspace.yaml. Auto-detected from running serve if omitted.
        #[arg(long)]
        config: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum SmAction {
    /// List defined models.
    Models,
    /// Show a model's states and transitions.
    Show { model: String },
    /// Create a new instance of a model.
    Create {
        model: String,
        title: String,
        #[arg(long)]
        body: Option<String>,
        /// Structured metadata as JSON string (e.g. '{"branch": "feat/x"}').
        #[arg(long)]
        metadata: Option<String>,
    },
    /// Move an instance to a new state.
    Move {
        id: String,
        state: String,
        #[arg(long)]
        note: Option<String>,
    },
    /// Show instance details and history.
    Status { id: String },
    /// List instances, optionally filtered.
    List {
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        state: Option<String>,
        #[arg(long, default_value = "50")]
        limit: usize,
    },
    /// Cancel an instance (move to terminal state if available).
    Cancel { id: String },
}

#[derive(Subcommand)]
pub enum GraphAction {
    /// Execute a skill graph from a YAML definition file.
    Run {
        /// Path to the graph YAML file.
        file: String,
        /// Working directory for tool execution (defaults to graph file's parent dir).
        #[arg(long)]
        work_dir: Option<String>,
        /// Input variables as key=value pairs (repeatable).
        #[arg(long = "var", value_name = "KEY=VALUE")]
        vars: Vec<String>,
    },
    /// Validate a graph YAML file without executing it (checks DAG, references, conditions).
    Validate {
        /// Path to the graph YAML file.
        file: String,
    },
}

#[derive(Subcommand)]
pub enum TaskAction {
    /// Add a task to the queue.
    Add {
        /// Task description.
        description: String,
        /// Required model (e.g. claude-sonnet-4-6).
        #[arg(long)]
        model: Option<String>,
        /// Required labels (comma-separated).
        #[arg(long, value_delimiter = ',')]
        labels: Vec<String>,
        /// Structured metadata as JSON string (e.g. '{"worktree": "/path"}').
        #[arg(long)]
        metadata: Option<String>,
    },
    /// List tasks in the queue.
    List {
        /// Filter by status: pending, active, done, failed, cancelled, dead_letter.
        #[arg(long)]
        status: Option<String>,
        /// Show only dead-lettered tasks (shorthand for --status dead_letter).
        #[arg(long)]
        dead_letter: bool,
    },
    /// Cancel a pending task.
    Cancel {
        /// Task ID (e.g. task-a1b2c3d4).
        id: String,
    },
}

#[derive(Subcommand)]
pub enum ScheduleSubcommand {
    /// List all schedules with cron, action, target, and next fire time.
    List,
    /// Add a new schedule.
    Add {
        /// Cron expression (6-field: sec min hour day month weekday).
        #[arg(long)]
        cron: String,
        /// Action type: github_poll, raw, or shell.
        #[arg(long)]
        action: String,
        /// Bus target (e.g. agent:dev).
        #[arg(long)]
        target: String,
        /// Action config as JSON string (e.g. '{"repos":["kgatilin/deskd"]}').
        #[arg(long)]
        config_json: Option<String>,
    },
    /// Remove a schedule by index (as shown in `list`).
    Rm {
        /// Schedule index (0-based).
        index: usize,
    },
}

#[derive(Subcommand)]
pub enum BusAction {
    /// Show bus state: connected clients and their subscriptions.
    Status {
        /// Bus socket path.
        #[arg(long, default_value = DEFAULT_SOCKET)]
        socket: String,
    },
    /// Start the bus API handler on an existing bus socket.
    ///
    /// Connects to a running bus and exposes the query/command API
    /// (agent_list, task_list, sm_list, etc.) so external tools like
    /// viewgraph can use the structured API without a full `deskd serve`.
    ///
    /// Examples:
    ///   deskd bus api --socket /home/kira/.deskd/bus.sock
    ///   DESKD_BUS_SOCKET=/tmp/bus.sock deskd bus api
    Api {
        /// Bus socket path. Defaults to $DESKD_BUS_SOCKET, or auto-discovered
        /// from running serve state.
        #[arg(long, env = "DESKD_BUS_SOCKET")]
        socket: Option<String>,
        /// Path to deskd.yaml for SM models and schedules.
        /// Defaults to $DESKD_AGENT_CONFIG, or auto-discovered from serve state.
        #[arg(long, env = "DESKD_AGENT_CONFIG")]
        config: Option<String>,
        /// Agent name for API responses. Defaults to $DESKD_AGENT_NAME or "cli".
        #[arg(long, env = "DESKD_AGENT_NAME")]
        agent: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum A2aAction {
    /// Generate and print the Agent Card JSON for this workspace.
    AgentCard {
        /// Path to workspace.yaml. Auto-detected from running serve if omitted.
        #[arg(long)]
        config: Option<String>,
    },
    /// Start the A2A HTTP server (Agent Card endpoint + JSON-RPC).
    Serve {
        /// Path to workspace.yaml. Auto-detected from running serve if omitted.
        #[arg(long)]
        config: Option<String>,
        /// Listen address override (default from workspace.yaml a2a.listen).
        #[arg(long)]
        listen: Option<String>,
    },
    /// Generate an Ed25519 key pair for JWT authentication.
    Keygen {},
}
