//! A pure, testable state machine for the watcher's wake/sleep decision.
//!
//! This replaces the old implicit `active` / `last_working` / `release_at`
//! bookkeeping with explicit states and transitions (see
//! `PLUGIN_IMPROVEMENT_PLAN.md`, "Add An Explicit State Machine"):
//!
//! ```text
//! Off          -- working --> PendingWake
//! PendingWake -- idle    --> Off
//! PendingWake -- sustained working past start_grace --> Awake
//! Awake        -- idle    --> PendingSleep
//! PendingSleep -- working --> Awake
//! PendingSleep -- sustained idle past stop_grace --> Off
//! Error        -- recovered --> Off, PendingWake, or Awake
//! ```
//!
//! `step` is pure: given the current input and a clock reading, it returns
//! what the caller should *do* (`Action`), and never touches a process, a
//! file, or the network itself. That is what makes flicker behavior (the
//! whole point of `start_grace`/`stop_grace`) straightforward to unit test.

use std::time::{Duration, Instant};

/// The watcher's wake/sleep state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum State {
    /// Not holding a wake assertion; no agent has been working recently.
    Off,
    /// An agent just started working; waiting out `start_grace` before
    /// acquiring, so a one-off flicker does not wake the machine.
    PendingWake,
    /// Holding a wake assertion.
    Awake,
    /// No agent is working right now, but we are still holding the
    /// assertion through `stop_grace`, so a brief idle flicker does not
    /// release it.
    PendingSleep,
    /// The last snapshot could not be fetched (Herdr unreachable, socket
    /// error, parse error, ...). Holding status is left exactly as it was
    /// until a snapshot succeeds again.
    Error,
}

impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            State::Off => "Off",
            State::PendingWake => "PendingWake",
            State::Awake => "Awake",
            State::PendingSleep => "PendingSleep",
            State::Error => "Error",
        };
        f.write_str(s)
    }
}

/// One evaluation's worth of input to the state machine.
#[derive(Clone, Copy, Debug)]
pub struct Input {
    /// Whether this evaluation's snapshot was fetched successfully at all.
    pub available: bool,
    /// Whether at least one agent currently matches the configured
    /// "working" statuses. Ignored when `available` is false.
    pub working: bool,
}

/// What the caller should do in response to a `step`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    /// No change: keep whatever is currently held (or not held).
    None,
    /// Start holding the wake assertion.
    Acquire,
    /// Stop holding the wake assertion.
    Release,
}

pub struct StateMachine {
    state: State,
    start_grace: Duration,
    stop_grace: Duration,
    /// When the current `PendingWake`/`PendingSleep` window began.
    pending_since: Option<Instant>,
    /// Whether we were holding the assertion (`Awake`/`PendingSleep`) right
    /// before entering `Error`, so recovery does not lose that fact.
    held_before_error: bool,
    /// The most recent reason recorded while transitioning into `Error`.
    last_error: Option<String>,
}

impl StateMachine {
    pub fn new(start_grace: Duration, stop_grace: Duration) -> Self {
        StateMachine {
            state: State::Off,
            start_grace,
            stop_grace,
            pending_since: None,
            held_before_error: false,
            last_error: None,
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// True if this state means "the wake assertion should currently be held".
    pub fn holding(&self) -> bool {
        matches!(self.state, State::Awake | State::PendingSleep)
            || (matches!(self.state, State::Error) && self.held_before_error)
    }

    /// When the current grace window (if any) is due to expire, so callers
    /// can size their sleep/poll interval instead of busy-waiting. `None`
    /// means there is no pending deadline (e.g. `Off` or `Awake` with no
    /// idle observed yet).
    pub fn deadline(&self) -> Option<Instant> {
        let since = self.pending_since?;
        match self.state {
            State::PendingWake => Some(since + self.start_grace),
            State::PendingSleep => Some(since + self.stop_grace),
            _ => None,
        }
    }

    /// Record that a snapshot failed, with a human-readable reason. Does not
    /// change holding status; only the `Error` state's bookkeeping.
    pub fn note_unavailable(&mut self, reason: impl Into<String>) {
        if !matches!(self.state, State::Error) {
            self.held_before_error = matches!(self.state, State::Awake | State::PendingSleep);
            self.state = State::Error;
            self.pending_since = None;
        }
        self.last_error = Some(reason.into());
    }

    /// Advance the state machine by one evaluation. Pure: does not perform
    /// any I/O; the caller is responsible for actually acquiring/releasing
    /// the assertion in response to the returned `Action`.
    pub fn step(&mut self, input: Input, now: Instant) -> Action {
        if !input.available {
            self.note_unavailable("snapshot unavailable");
            return Action::None;
        }

        if matches!(self.state, State::Error) {
            self.last_error = None;
            self.state = if self.held_before_error {
                State::Awake
            } else {
                State::Off
            };
            self.pending_since = None;
            // Re-dispatch immediately so the freshly-recovered state reacts
            // to `input` in the same evaluation (e.g. Awake -> PendingSleep
            // if idle, or Off -> PendingWake if working), instead of waiting
            // for the next tick to notice.
            return self.step(input, now);
        }

        match self.state {
            State::Off => {
                if input.working {
                    self.state = State::PendingWake;
                    self.pending_since = Some(now);
                }
                Action::None
            }
            State::PendingWake => {
                if !input.working {
                    self.state = State::Off;
                    self.pending_since = None;
                    return Action::None;
                }
                let since = *self.pending_since.get_or_insert(now);
                if now.saturating_duration_since(since) >= self.start_grace {
                    self.state = State::Awake;
                    self.pending_since = None;
                    Action::Acquire
                } else {
                    Action::None
                }
            }
            State::Awake => {
                if !input.working {
                    self.state = State::PendingSleep;
                    self.pending_since = Some(now);
                }
                Action::None
            }
            State::PendingSleep => {
                if input.working {
                    self.state = State::Awake;
                    self.pending_since = None;
                    return Action::None;
                }
                let since = *self.pending_since.get_or_insert(now);
                if now.saturating_duration_since(since) >= self.stop_grace {
                    self.state = State::Off;
                    self.pending_since = None;
                    Action::Release
                } else {
                    Action::None
                }
            }
            State::Error => unreachable!("Error is fully handled above"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn feed(sm: &mut StateMachine, t0: Instant, timeline: &[(u64, bool)]) -> Vec<Action> {
        timeline
            .iter()
            .map(|(offset_secs, working)| {
                sm.step(
                    Input {
                        available: true,
                        working: *working,
                    },
                    t0 + secs(*offset_secs),
                )
            })
            .collect()
    }

    #[test]
    fn starts_off() {
        let sm = StateMachine::new(secs(5), secs(30));
        assert_eq!(sm.state(), State::Off);
        assert!(!sm.holding());
    }

    /// Acceptance criterion: a one-second `working` flicker does not acquire
    /// a wake assertion.
    #[test]
    fn short_working_flicker_never_acquires() {
        let mut sm = StateMachine::new(secs(5), secs(30));
        let t0 = Instant::now();
        let actions = feed(&mut sm, t0, &[(0, true), (1, false), (2, false)]);
        assert!(actions.iter().all(|a| *a == Action::None));
        assert_eq!(sm.state(), State::Off);
        assert!(!sm.holding());
    }

    /// Acceptance criterion: sustained `working` acquires after `start_grace`.
    #[test]
    fn sustained_working_acquires_after_start_grace() {
        let mut sm = StateMachine::new(secs(5), secs(30));
        let t0 = Instant::now();
        let actions = feed(
            &mut sm,
            t0,
            &[(0, true), (1, true), (3, true), (5, true), (6, true)],
        );
        assert_eq!(
            actions,
            vec![
                Action::None,
                Action::None,
                Action::None,
                Action::Acquire,
                Action::None,
            ]
        );
        assert_eq!(sm.state(), State::Awake);
        assert!(sm.holding());
    }

    /// Acceptance criterion: a short idle flicker during `Awake` does not
    /// release before `stop_grace`.
    #[test]
    fn short_idle_flicker_during_awake_does_not_release() {
        let mut sm = StateMachine::new(secs(5), secs(30));
        let t0 = Instant::now();
        // Get to Awake first.
        feed(&mut sm, t0, &[(0, true), (5, true)]);
        assert_eq!(sm.state(), State::Awake);

        // Brief idle blip, then working resumes well before stop_grace.
        let actions = feed(&mut sm, t0, &[(6, false), (10, false), (15, true)]);
        assert!(actions.iter().all(|a| *a == Action::None));
        assert_eq!(sm.state(), State::Awake);
        assert!(sm.holding());
    }

    /// Acceptance criterion: sustained idle releases after `stop_grace`.
    #[test]
    fn sustained_idle_releases_after_stop_grace() {
        let mut sm = StateMachine::new(secs(5), secs(30));
        let t0 = Instant::now();
        feed(&mut sm, t0, &[(0, true), (5, true)]);
        assert_eq!(sm.state(), State::Awake);

        let actions = feed(&mut sm, t0, &[(6, false), (20, false), (36, false)]);
        assert_eq!(actions, vec![Action::None, Action::None, Action::Release]);
        assert_eq!(sm.state(), State::Off);
        assert!(!sm.holding());
    }

    #[test]
    fn pending_wake_resets_grace_window_after_returning_to_off() {
        let mut sm = StateMachine::new(secs(5), secs(30));
        let t0 = Instant::now();
        // First attempt flickers away.
        feed(&mut sm, t0, &[(0, true), (2, false)]);
        assert_eq!(sm.state(), State::Off);

        // A second, sustained attempt should need its own full start_grace
        // window, not benefit from the first attempt's elapsed time.
        let actions = feed(&mut sm, t0, &[(3, true), (7, true), (8, true), (9, true)]);
        assert_eq!(
            actions,
            vec![Action::None, Action::None, Action::Acquire, Action::None]
        );
        assert_eq!(sm.state(), State::Awake);
    }

    #[test]
    fn unavailable_snapshot_holds_and_recovers_to_awake() {
        let mut sm = StateMachine::new(secs(5), secs(30));
        let t0 = Instant::now();
        feed(&mut sm, t0, &[(0, true), (5, true)]);
        assert_eq!(sm.state(), State::Awake);

        // Herdr goes unreachable for a while; must not release.
        let a1 = sm.step(
            Input {
                available: false,
                working: false,
            },
            t0 + secs(10),
        );
        assert_eq!(a1, Action::None);
        assert_eq!(sm.state(), State::Error);
        assert!(sm.holding());
        assert!(sm.last_error().is_some());

        // Recovers, still working: should be back to Awake without a fresh
        // acquire (we never actually released).
        let a2 = sm.step(
            Input {
                available: true,
                working: true,
            },
            t0 + secs(15),
        );
        assert_eq!(a2, Action::None);
        assert_eq!(sm.state(), State::Awake);
        assert!(sm.last_error().is_none());
    }

    #[test]
    fn unavailable_snapshot_from_off_recovers_to_pending_wake() {
        let mut sm = StateMachine::new(secs(5), secs(30));
        let t0 = Instant::now();
        assert_eq!(sm.state(), State::Off);

        sm.step(
            Input {
                available: false,
                working: false,
            },
            t0,
        );
        assert_eq!(sm.state(), State::Error);
        assert!(!sm.holding());

        // Recovers with a working agent: goes through Off -> PendingWake,
        // i.e. still has to serve a fresh start_grace, not acquire instantly.
        let action = sm.step(
            Input {
                available: true,
                working: true,
            },
            t0 + secs(1),
        );
        assert_eq!(action, Action::None);
        assert_eq!(sm.state(), State::PendingWake);
    }
}
