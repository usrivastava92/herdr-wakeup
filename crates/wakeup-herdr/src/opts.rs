//! CLI + environment parsing for wakeup-herdr.

use crate::persist;
use std::time::Duration;

pub struct Opts {
    pub socket: String,
    pub herdr_bin: String,
    pub wakeup_bin: String,
    pub display: bool,
    pub statuses: Vec<String>,
    /// Minimum sustained "working" time before acquiring the wake assertion.
    pub start_grace: Duration,
    /// Minimum sustained "idle" time before releasing the wake assertion.
    /// This is the original `--grace` flag; the name is kept for backward
    /// compatibility with existing scripts/docs.
    pub grace: Duration,
    pub backstop: Duration,
    pub debounce: Duration,
    pub exit_after: Duration,
    pub no_notify: bool,
    pub verbose: bool,
    pub quiet: bool,
    pub once: bool,
    /// If the Herdr socket itself is unreachable, shell out to `herdr agent
    /// list` instead of just reporting "unavailable". Off by default: normal
    /// operation never spawns a Herdr process (see Milestone 3).
    pub allow_cli_fallback: bool,
}

const USAGE: &str = "\
wakeup-herdr - keep macOS awake while a Herdr agent is working (event-driven)

USAGE:
    wakeup-herdr [options]

It connects to Herdr's socket, subscribes to lifecycle + agent status events,
and fetches snapshots via the socket agent.list RPC (no herdr process is
spawned during normal operation): if any agent is working it runs wakeup,
otherwise it lets the machine sleep. If the socket itself is unreachable it
just reports unavailable and retries, unless --allow-cli-fallback is set.

A JSON config file (config.json in $HERDR_PLUGIN_CONFIG_DIR, or
~/.config/herdr-wakeup standalone) seeds these defaults below the CLI flags;
see `wakeup-herdr arm`/`disarm` and `wakeup-herdr state` for the config/state
subcommands.

OPTIONS:
    -d, --display        Also keep the display awake (runs `wakeup -di`).
    --start-grace <secs> Require this much sustained working time before acquiring (default 5).
    --grace <secs>       Stay awake this long after the last working agent (default 30).
    --backstop <secs>    Safety re-evaluation interval (default 60; 0 disables extra ticks).
    --exit-after <secs>  Exit if the Herdr server stays unreachable this long (default 120; 0 = never exit).
    --debounce <ms>      Coalesce event bursts within this window (default 400).
    --statuses <list>    Comma-separated statuses that count as active (default: working).
    --socket <path>      Herdr socket path (default: $HERDR_SOCKET_PATH or ~/.config/herdr/herdr.sock).
    --wakeup <path>      Path to the wakeup binary (default: $WAKEUP_BIN or `wakeup`).
    --herdr <path>       Path to the herdr binary (default: $HERDR_BIN_PATH or `herdr`).
    --allow-cli-fallback Shell out to `herdr agent list` if the socket itself is unreachable.
    --no-notify          Do not post wake/sleep toast notifications.
    --once               Print the current decision and exit (debug; always uses the CLI).
    -v, --verbose        Log every evaluation.
    -q, --quiet          Suppress routine logging.
    -h, --help           Show this help.
    -V, --version        Show version.
";

impl Opts {
    pub fn parse() -> Opts {
        // Config file values seed the defaults below (precedence: built-in <
        // config file < env var < CLI flag). A missing config file is
        // bootstrapped with the full set of defaults right here (so it is
        // immediately present and editable after the very first run, not
        // only as a side effect of `arm`/`disarm`); a corrupt one falls back
        // safely in memory and is reported, without touching the file on
        // disk, per Milestone 4's "corrupt config falls back safely and
        // reports an error" acceptance criterion. `armed` itself is
        // intentionally not read into Opts: the watcher re-reads it live on
        // every evaluation (see App::reload_armed) so `disarm`/`arm` take
        // effect without a restart, instead of only being read once at
        // startup.
        let (cfg, cfg_err) = persist::Config::ensure_bootstrapped(&persist::config_path());
        if let Some(e) = &cfg_err {
            eprintln!("wakeup-herdr: config error (using defaults): {e}");
        }

        let home = std::env::var("HOME").unwrap_or_default();
        let default_socket = std::env::var("HERDR_SOCKET_PATH")
            .unwrap_or_else(|_| format!("{home}/.config/herdr/herdr.sock"));
        let default_herdr = std::env::var("HERDR_BIN_PATH")
            .or_else(|_| std::env::var("WAKEUP_HERDR_BIN"))
            .unwrap_or(cfg.herdr_bin);
        let default_wakeup = std::env::var("WAKEUP_BIN").unwrap_or(cfg.wakeup_bin);

        let mut o = Opts {
            socket: default_socket,
            herdr_bin: default_herdr,
            wakeup_bin: default_wakeup,
            display: cfg.display,
            statuses: cfg.statuses,
            start_grace: Duration::from_secs(cfg.start_grace_seconds),
            grace: Duration::from_secs(cfg.stop_grace_seconds),
            backstop: Duration::from_secs(60),
            debounce: Duration::from_millis(400),
            exit_after: Duration::from_secs(120),
            no_notify: !cfg.notify,
            verbose: false,
            quiet: false,
            once: false,
            allow_cli_fallback: cfg.allow_cli_fallback,
        };

        let mut args = std::env::args().skip(1);
        while let Some(a) = args.next() {
            match a.as_str() {
                "-h" | "--help" => {
                    print!("{USAGE}");
                    std::process::exit(0);
                }
                "-V" | "--version" => {
                    println!("wakeup-herdr {}", env!("CARGO_PKG_VERSION"));
                    std::process::exit(0);
                }
                "-d" | "--display" => o.display = true,
                "--start-grace" => o.start_grace = duration_secs(args.next(), "--start-grace"),
                "--grace" => o.grace = duration_secs(args.next(), "--grace"),
                "--backstop" => o.backstop = duration_secs(args.next(), "--backstop"),
                "--exit-after" => o.exit_after = duration_secs(args.next(), "--exit-after"),
                "--debounce" => o.debounce = duration_millis(args.next(), "--debounce"),
                "--statuses" => {
                    o.statuses = val(args.next(), "--statuses")
                        .split(',')
                        .map(|s| s.trim().to_lowercase())
                        .filter(|s| !s.is_empty())
                        .collect();
                    if o.statuses.is_empty() {
                        fail("--statuses requires at least one non-empty status");
                    }
                }
                "--socket" => o.socket = val(args.next(), "--socket"),
                "--wakeup" => o.wakeup_bin = val(args.next(), "--wakeup"),
                "--herdr" => o.herdr_bin = val(args.next(), "--herdr"),
                "--allow-cli-fallback" => o.allow_cli_fallback = true,
                "--no-notify" => o.no_notify = true,
                "--once" => o.once = true,
                "-v" | "--verbose" => o.verbose = true,
                "-q" | "--quiet" => o.quiet = true,
                other => fail(&format!("unknown option: {other}")),
            }
        }
        // A zero backstop means "no periodic ticks": use a very long interval.
        if o.backstop.is_zero() {
            o.backstop = Duration::from_secs(24 * 3600);
        }
        o
    }
}

fn val(v: Option<String>, flag: &str) -> String {
    v.unwrap_or_else(|| fail(&format!("{flag} requires a value")))
}

fn duration_secs(v: Option<String>, flag: &str) -> Duration {
    let seconds = num(v, flag);
    Duration::try_from_secs_f64(seconds)
        .unwrap_or_else(|_| fail(&format!("{flag} is out of range")))
}

fn duration_millis(v: Option<String>, flag: &str) -> Duration {
    let millis = num(v, flag);
    Duration::try_from_secs_f64(millis / 1000.0)
        .unwrap_or_else(|_| fail(&format!("{flag} is out of range")))
}

fn num(v: Option<String>, flag: &str) -> f64 {
    let s = val(v, flag);
    let n = s
        .parse::<f64>()
        .unwrap_or_else(|_| fail(&format!("{flag} expects a number, got {s:?}")));
    if !n.is_finite() || n < 0.0 {
        fail(&format!(
            "{flag} expects a non-negative finite number, got {s:?}"
        ));
    }
    n
}
fn fail(msg: &str) -> ! {
    eprintln!("wakeup-herdr: {msg}");
    eprintln!("try `wakeup-herdr --help`");
    std::process::exit(2);
}
