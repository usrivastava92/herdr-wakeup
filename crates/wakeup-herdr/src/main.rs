//! wakeup-herdr - a Herdr plugin that keeps macOS awake while agents are working.
//!
//! Design: this is a small resident process that subscribes to Herdr's socket
//! event stream (`pane.agent_status_changed` + pane lifecycle). On *any* event it
//! re-queries the authoritative `herdr agent list` and feeds the result into a
//! pure state machine (see `state.rs`):
//!   - working starts, sustained past `--start-grace` -> acquire the `wakeup` assertion
//!   - working stops, sustained past `--grace`         -> release it and let the machine sleep
//!   - brief flickers in either direction do nothing, by design
//!
//! It never tracks per-agent state and never parses event payloads; an event is
//! just a "re-evaluate now" trigger. Policy lives here, mechanism lives in the
//! `wakeup` binary (spawned as `wakeup -i -w <our-pid>`, so it self-releases if we
//! ever die). If the socket is unavailable it transparently falls back to polling.
//!
//! It runs in the background (no pane) and surfaces state as toast notifications
//! on transitions: "☕ Keeping awake" when it starts holding the assertion and
//! "💤 Sleep allowed" when it releases.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

mod opts;
use opts::Opts;

mod state;
use state::{Action, Input, StateMachine};

// Set by the signal handler so the main loop can clean up promptly.
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: i32) {
    STOP.store(true, Ordering::SeqCst);
}

extern "C" {
    fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
}

fn install_signals() {
    // SIGINT=2, SIGHUP=1, SIGTERM=15 on macOS.
    unsafe {
        signal(2, on_signal);
        signal(1, on_signal);
        signal(15, on_signal);
    }
}

fn stopping() -> bool {
    STOP.load(Ordering::SeqCst)
}

fn main() {
    let o = Opts::parse();

    if o.once {
        let snap = herdr_agents(&o);
        report_once(&snap, &o);
        return;
    }

    install_signals();
    let mut app = App::new(o);
    app.log(&format!(
        "start pid={} socket={} start_grace={:.0}s stop_grace={:.0}s backstop={:.0}s display={}",
        std::process::id(),
        app.o.socket,
        app.o.start_grace.as_secs_f64(),
        app.o.grace.as_secs_f64(),
        app.o.backstop.as_secs_f64(),
        app.o.display,
    ));
    app.run();
}

// --------------------------------------------------------------------------- //
// Snapshot of Herdr agents
// --------------------------------------------------------------------------- //

struct Snapshot {
    available: bool,
    working: Vec<String>,    // labels of working agents
    panes: BTreeSet<String>, // pane ids of all detected agents
    total: usize,
}

fn herdr_agents(o: &Opts) -> Snapshot {
    let out = run_capture(&o.herdr_bin, &["agent", "list"], Duration::from_secs(8));
    let out = match out {
        Some(s) if !s.trim().is_empty() => s,
        _ => {
            return Snapshot {
                available: false,
                working: vec![],
                panes: BTreeSet::new(),
                total: 0,
            }
        }
    };
    let val: serde_json::Value = match serde_json::from_str(&out) {
        Ok(v) => v,
        Err(_) => {
            return Snapshot {
                available: false,
                working: vec![],
                panes: BTreeSet::new(),
                total: 0,
            }
        }
    };
    let agents = val
        .get("result")
        .and_then(|r| r.get("agents"))
        .and_then(|a| a.as_array());
    let agents = match agents {
        Some(a) => a,
        None => {
            return Snapshot {
                available: false,
                working: vec![],
                panes: BTreeSet::new(),
                total: 0,
            }
        }
    };
    let mut working = Vec::new();
    let mut panes = BTreeSet::new();
    for a in agents {
        if let Some(p) = a.get("pane_id").and_then(|v| v.as_str()) {
            panes.insert(p.to_string());
        }
        let status = a
            .get("agent_status")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_lowercase();
        if o.statuses.iter().any(|s| s == &status) {
            working.push(agent_label(a));
        }
    }
    Snapshot {
        available: true,
        working,
        panes,
        total: agents.len(),
    }
}

fn agent_label(a: &serde_json::Value) -> String {
    let name = a
        .get("agent")
        .and_then(|v| v.as_str())
        .or_else(|| a.get("terminal_id").and_then(|v| v.as_str()))
        .unwrap_or("agent");
    let cwd = a
        .get("foreground_cwd")
        .and_then(|v| v.as_str())
        .or_else(|| a.get("cwd").and_then(|v| v.as_str()))
        .unwrap_or("");
    let tail = cwd.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    if tail.is_empty() {
        name.to_string()
    } else {
        format!("{name}@{tail}")
    }
}

fn report_once(snap: &Snapshot, o: &Opts) {
    if !snap.available {
        println!("herdr: unavailable");
    } else {
        println!(
            "herdr: {} agent(s), {} matching [{}]",
            snap.total,
            snap.working.len(),
            o.statuses.join(",")
        );
        for w in &snap.working {
            println!("  working: {w}");
        }
    }
    println!(
        "decision: {}",
        if snap.working.is_empty() {
            "allow sleep"
        } else {
            "keep awake"
        }
    );
}

// --------------------------------------------------------------------------- //
// wakeup child management
// --------------------------------------------------------------------------- //

struct Keeper {
    bin: String,
    args: Vec<String>,
    child: Option<Child>,
}

impl Keeper {
    fn new(bin: String, display: bool) -> Self {
        let pid = std::process::id();
        let mut args: Vec<String> = Vec::new();
        args.push(if display { "-di".into() } else { "-i".into() });
        args.push("-w".into());
        args.push(pid.to_string());
        Keeper {
            bin,
            args,
            child: None,
        }
    }
    fn running(&mut self) -> bool {
        match &mut self.child {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    }
    fn start(&mut self) -> std::io::Result<()> {
        self.child = Some(Command::new(&self.bin).args(&self.args).spawn()?);
        Ok(())
    }
    fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

// --------------------------------------------------------------------------- //
// App
// --------------------------------------------------------------------------- //

struct App {
    o: Opts,
    keeper: Keeper,
    sm: StateMachine,
    next_eval: Instant, // next self-initiated backstop evaluation
    subscribed: BTreeSet<String>,
    unavailable_since: Option<Instant>, // first time Herdr became unreachable
    herdr_gone: bool,                   // Herdr unreachable past --exit-after: time to quit
}

enum Poll {
    Activity,
    Timeout,
    Closed,
}

impl App {
    fn new(o: Opts) -> Self {
        let keeper = Keeper::new(o.wakeup_bin.clone(), o.display);
        let sm = StateMachine::new(o.start_grace, o.grace);
        App {
            o,
            keeper,
            sm,
            next_eval: Instant::now(),
            subscribed: BTreeSet::new(),
            unavailable_since: None,
            herdr_gone: false,
        }
    }

    fn log(&self, msg: &str) {
        if self.o.quiet {
            return;
        }
        println!("{} {msg}", now_ts());
        let _ = std::io::stdout().flush();
    }

    fn run(&mut self) {
        while !stopping() && !self.herdr_gone {
            let panes = herdr_agents(&self.o).panes;
            match self.connect_subscribe(&panes) {
                Some(mut stream) => {
                    self.subscribed = panes;
                    self.evaluate("connected");
                    if self.herdr_gone {
                        break;
                    }
                    self.next_eval = Instant::now() + self.o.backstop;
                    self.stream_loop(&mut stream);
                    // fell out: reconnect (or stopping)
                }
                None => {
                    // No socket subscription right now: do one polling evaluation,
                    // then fall back to the outer loop and retry connect_subscribe,
                    // so event-driven mode is restored as soon as Herdr is reachable
                    // again (e.g. after a quick server restart). Retry sooner while
                    // Herdr is unreachable; otherwise at the backstop interval.
                    self.evaluate("poll");
                    if self.herdr_gone {
                        break;
                    }
                    let nap = if self.unavailable_since.is_some() {
                        Duration::from_secs(5)
                    } else {
                        self.o.backstop
                    };
                    let deadline = Instant::now() + nap;
                    while !stopping() && !self.herdr_gone && Instant::now() < deadline {
                        let left = deadline.saturating_duration_since(Instant::now());
                        std::thread::sleep(Duration::from_millis(500).min(left));
                    }
                }
            }
            if stopping() || self.herdr_gone {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        self.shutdown();
    }

    /// Read events until the pane set changes, the connection drops, or we stop.
    fn stream_loop(&mut self, stream: &mut Conn) {
        loop {
            if stopping() {
                return;
            }
            // Wake at least once a second so we notice signals promptly, but
            // only re-query Herdr on real events or when a deadline elapses.
            let to = Duration::from_millis(1000).min(self.until_deadline());
            match stream.wait(to) {
                Poll::Closed => {
                    self.log("socket closed; reconnecting");
                    return;
                }
                Poll::Timeout => {
                    if stopping() {
                        return;
                    }
                    if self.deadline_passed() {
                        self.evaluate("tick");
                        if self.herdr_gone {
                            return;
                        }
                        self.next_eval = Instant::now() + self.o.backstop;
                    }
                }
                Poll::Activity => {
                    stream.drain(self.o.debounce);
                    let changed = self.evaluate("event");
                    if self.herdr_gone {
                        return;
                    }
                    self.next_eval = Instant::now() + self.o.backstop;
                    if changed {
                        // pane membership changed -> reconnect to re-subscribe.
                        return;
                    }
                }
            }
        }
    }

    fn until_deadline(&self) -> Duration {
        let now = Instant::now();
        let mut d = self.next_eval.saturating_duration_since(now);
        if let Some(r) = self.sm.deadline() {
            d = d.min(r.saturating_duration_since(now));
        }
        d
    }

    fn deadline_passed(&self) -> bool {
        let now = Instant::now();
        now >= self.next_eval || self.sm.deadline().map(|r| now >= r).unwrap_or(false)
    }

    fn shutdown(&mut self) {
        self.keeper.stop();
        self.log("stopped; assertion released");
    }

    /// Re-query Herdr and toggle wakeup. Returns true if the pane set changed
    /// (so the caller knows to re-subscribe).
    fn evaluate(&mut self, reason: &str) -> bool {
        let snap = herdr_agents(&self.o);
        let now = Instant::now();

        // Tie our lifecycle to the Herdr server: if it stays unreachable past the
        // tolerance window (surviving brief blips and restarts), quit. There are
        // no agents to track without Herdr, so lingering would be pointless.
        if snap.available {
            self.unavailable_since = None;
        } else if !self.o.exit_after.is_zero() {
            let since = *self.unavailable_since.get_or_insert(now);
            if now.duration_since(since) >= self.o.exit_after {
                self.log(&format!(
                    "[{reason}] herdr unreachable for {:.0}s; exiting",
                    now.duration_since(since).as_secs_f64()
                ));
                self.herdr_gone = true;
                return false;
            }
        }

        let working = !snap.working.is_empty();
        let action = self.sm.step(
            Input {
                available: snap.available,
                working,
            },
            now,
        );

        if self.o.verbose {
            self.log(&format!(
                "[{reason}] working=[{}] state={} action={:?}{}",
                snap.working.join(", "),
                self.sm.state(),
                action,
                self.sm
                    .last_error()
                    .map(|e| format!(" last_error={e:?}"))
                    .unwrap_or_default(),
            ));
        }

        match action {
            Action::Acquire => match self.keeper.start() {
                Ok(()) => {
                    let detail = summarize(&snap.working);
                    self.log(&format!("AWAKE   -> {detail}"));
                    self.toast("☕ Keeping awake", &detail);
                }
                Err(e) => self.log(&format!("failed to start wakeup: {e}")),
            },
            Action::Release => {
                self.keeper.stop();
                self.log("SLEEP-OK <- all agents idle/blocked");
                self.toast("💤 Sleep allowed", "no agents working");
            }
            Action::None => {
                // No transition this round, but make sure the assertion is
                // actually still held whenever the state machine believes it
                // should be (recovers a crashed/killed `wakeup` child).
                if self.sm.holding() && !self.keeper.running() {
                    match self.keeper.start() {
                        Ok(()) => self.log("restarted wakeup (child had exited)"),
                        Err(e) => self.log(&format!("failed to restart wakeup: {e}")),
                    }
                }
            }
        }

        // report pane-set change for re-subscribe
        snap.available && snap.panes != self.subscribed
    }

    // ----- Herdr visual surfaces ----- //

    fn toast(&self, title: &str, body: &str) {
        if self.o.no_notify {
            return;
        }
        let _ = run_status(
            &self.o.herdr_bin,
            &[
                "notification",
                "show",
                title,
                "--body",
                body,
                "--sound",
                "none",
            ],
        );
    }

    /// Connect to the socket and subscribe to lifecycle + per-pane agent status.
    fn connect_subscribe(&self, panes: &BTreeSet<String>) -> Option<Conn> {
        let stream = UnixStream::connect(&self.o.socket).ok()?;
        let mut conn = Conn::new(stream);

        let mut subs = vec![
            serde_json::json!({"type": "pane.created"}),
            serde_json::json!({"type": "pane.exited"}),
            serde_json::json!({"type": "pane.agent_detected"}),
        ];
        for p in panes {
            subs.push(serde_json::json!({"type": "pane.agent_status_changed", "pane_id": p}));
        }
        let req = serde_json::json!({
            "id": "wakeup-sub",
            "method": "events.subscribe",
            "params": {"subscriptions": subs},
        });
        conn.send(&req).ok()?;
        // Expect the explicit subscription_started ack within a short window.
        match conn.recv_json(Duration::from_secs(3)) {
            Some(v) if subscription_started(&v) => Some(conn),
            _ => None,
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.keeper.stop();
    }
}

fn summarize(working: &[String]) -> String {
    let n = working.len();
    let head: Vec<&str> = working.iter().take(3).map(|s| s.as_str()).collect();
    let mut s = format!("{n} working: {}", head.join(", "));
    if n > 3 {
        s.push_str(&format!(", +{}", n - 3));
    }
    s
}

// --------------------------------------------------------------------------- //
// Socket connection: newline-delimited JSON
// --------------------------------------------------------------------------- //

struct Conn {
    stream: UnixStream,
    buf: Vec<u8>,
}

impl Conn {
    fn new(stream: UnixStream) -> Self {
        Conn {
            stream,
            buf: Vec::new(),
        }
    }

    fn send(&mut self, v: &serde_json::Value) -> std::io::Result<()> {
        let mut line = serde_json::to_vec(v).unwrap();
        line.push(b'\n');
        self.stream.write_all(&line)
    }

    fn recv_json(&mut self, timeout: Duration) -> Option<serde_json::Value> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(line) = self.pop_line() {
                return serde_json::from_slice::<serde_json::Value>(&line).ok();
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            if matches!(
                self.wait(deadline.saturating_duration_since(now)),
                Poll::Closed
            ) {
                return None;
            }
        }
    }

    /// Block up to `timeout` for at least one complete message.
    fn wait(&mut self, timeout: Duration) -> Poll {
        if self.has_line() {
            return Poll::Activity;
        }
        let _ = self.stream.set_read_timeout(Some(timeout));
        let mut tmp = [0u8; 8192];
        match self.stream.read(&mut tmp) {
            Ok(0) => Poll::Closed,
            Ok(n) => {
                self.buf.extend_from_slice(&tmp[..n]);
                if self.has_line() {
                    Poll::Activity
                } else {
                    Poll::Timeout
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                Poll::Timeout
            }
            Err(_) => Poll::Closed,
        }
    }

    /// Swallow any further buffered/immediately-available messages so a burst of
    /// events collapses into a single evaluation.
    fn drain(&mut self, window: Duration) {
        self.consume_lines();
        let _ = self.stream.set_read_timeout(Some(window));
        let mut tmp = [0u8; 8192];
        loop {
            match self.stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    self.buf.extend_from_slice(&tmp[..n]);
                    self.consume_lines();
                }
                Err(_) => break,
            }
        }
    }

    fn has_line(&self) -> bool {
        self.buf.contains(&b'\n')
    }

    fn consume_lines(&mut self) {
        while self.pop_line().is_some() {}
    }

    fn pop_line(&mut self) -> Option<Vec<u8>> {
        if let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=i).collect();
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            Some(line)
        } else {
            None
        }
    }
}

fn subscription_started(v: &serde_json::Value) -> bool {
    v.get("id").and_then(|id| id.as_str()) == Some("wakeup-sub")
        && v.get("result")
            .and_then(|r| r.get("type"))
            .and_then(|t| t.as_str())
            == Some("subscription_started")
}

// --------------------------------------------------------------------------- //
// small process helpers
// --------------------------------------------------------------------------- //

fn run_capture(bin: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let child = Command::new(bin)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    let mut child = child;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut s = String::new();
                if let Some(mut out) = child.stdout.take() {
                    let _ = out.read_to_string(&mut s);
                }
                return if status.success() { Some(s) } else { None };
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(30));
            }
            Err(_) => return None,
        }
    }
}

fn run_status(bin: &str, args: &[&str]) -> bool {
    Command::new(bin)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Local wall-clock HH:MM:SS for log lines, computed without spawning a process
/// per log. The local UTC offset is learned once (one `date +%z` at startup) and
/// cached; everything after is pure arithmetic on the system clock.
fn now_ts() -> String {
    use std::sync::OnceLock;
    use std::time::{SystemTime, UNIX_EPOCH};

    static OFFSET: OnceLock<i64> = OnceLock::new();
    let offset = *OFFSET.get_or_init(|| {
        run_capture("date", &["+%z"], Duration::from_secs(2))
            .and_then(|s| parse_utc_offset(s.trim()))
            .unwrap_or(0)
    });
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let day = (epoch + offset).rem_euclid(86_400);
    format!("{:02}:{:02}:{:02}", day / 3600, (day % 3600) / 60, day % 60)
}

fn parse_utc_offset(z: &str) -> Option<i64> {
    // e.g. "+0530" or "-0800"
    if z.len() < 5 {
        return None;
    }
    let sign = if z.starts_with('-') { -1 } else { 1 };
    let h: i64 = z.get(1..3)?.parse().ok()?;
    let m: i64 = z.get(3..5)?.parse().ok()?;
    Some(sign * (h * 3600 + m * 60))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_subscription_ack() {
        let v = serde_json::json!({
            "id": "wakeup-sub",
            "result": {"type": "subscription_started"}
        });
        assert!(subscription_started(&v));
    }

    #[test]
    fn rejects_unrelated_socket_activity_as_ack() {
        let v = serde_json::json!({
            "type": "pane.agent_status_changed",
            "pane_id": "w1:p1"
        });
        assert!(!subscription_started(&v));
    }

    #[test]
    fn parses_and_removes_one_buffered_line() {
        let (client, mut server) = UnixStream::pair().unwrap();
        server.write_all(b"{\"one\":1}\n{\"two\":2}\n").unwrap();
        let mut conn = Conn::new(client);

        let first = conn.recv_json(Duration::from_secs(1)).unwrap();
        assert_eq!(first.get("one").and_then(|v| v.as_i64()), Some(1));
        assert!(conn.has_line());

        let second = conn.recv_json(Duration::from_secs(1)).unwrap();
        assert_eq!(second.get("two").and_then(|v| v.as_i64()), Some(2));
    }
}
