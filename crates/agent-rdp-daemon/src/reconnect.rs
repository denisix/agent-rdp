//! Re-establishing the RDP session by itself after a transport drop.
//!
//! The transport is the one layer in this system that never self-heals. The
//! agent relaunches, adoption works, the idempotency journal survives a
//! reboot - but a dropped connection sits there until a human types
//! `connect`. Overnight there is no human, and everything else in that
//! Windows session goes with it: a field team lost two mandatory measurement
//! cells and 8.3 hours of a benchmark to a connection that died at 23:43 and
//! was not noticed until morning.
//!
//! Opt-in, because recovery is not free. If the agent survived the outage it
//! is adopted silently; if it did not, bringing it back types Win+R on the
//! remote desktop.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use agent_rdp_protocol::ConnectRequest;

/// Backoff between attempts, by attempt number. Capped so an overnight
/// outage keeps trying without hammering a host that is simply gone.
pub fn backoff(attempt: u32) -> Duration {
    let secs = match attempt {
        0 | 1 => 5,
        2 => 10,
        3 => 20,
        4 => 40,
        _ => 60,
    };
    Duration::from_secs(secs)
}

/// How long a reconnected session must survive to count as a real recovery.
///
/// A host that accepts the connection and drops it again immediately would
/// otherwise be reconnected to forever, and each cycle can cost a Win+R on
/// the desktop.
pub const HEALTHY_SESSION: Duration = Duration::from_secs(120);

/// Consecutive too-short sessions before giving up. The backoff alone does
/// not help here: every attempt "succeeds", so the counter never resets.
pub const MAX_SHORT_SESSIONS: u32 = 5;

/// What the reconnect decision depends on, captured at one instant.
#[derive(Debug, Clone, Copy)]
pub struct ReconnectSnapshot {
    pub enabled: bool,
    pub have_request: bool,
    pub stopped: bool,
    pub in_flight: bool,
    /// The generation this outage belongs to, against the current one.
    pub dropped_generation: u64,
    pub current_generation: u64,
    /// The user-action counter at the time the outage was noticed, against
    /// its value now.
    pub serial_at_arm: u64,
    pub serial_now: u64,
}

/// Whether to attempt a reconnect, or why not.
///
/// Every guard is re-evaluated before each attempt, not once at the start:
/// an outage lasts minutes and the user can disconnect, connect, or shut the
/// daemon down at any point during it.
pub fn should_attempt(s: &ReconnectSnapshot) -> Result<(), &'static str> {
    if !s.enabled {
        return Err("auto-reconnect is not enabled for this session");
    }
    if !s.have_request {
        return Err("no connect request was retained");
    }
    if s.stopped {
        return Err("auto-reconnect gave up earlier and will not retry on its own");
    }
    if s.in_flight {
        return Err("a reconnect attempt is already running");
    }
    if s.current_generation != s.dropped_generation {
        return Err("a newer session replaced the one that dropped");
    }
    // The decisive guard. `disconnect` queues its command onto the same
    // channel the frame processor is servicing, so when someone disconnects
    // *because* the link is sick the read arm usually errors first and the
    // processor exits by the failure path - sending a drop event for a
    // session the user just closed. The graceful-shutdown flag cannot see
    // that race; a counter bumped by the user's own request can.
    if s.serial_now != s.serial_at_arm {
        return Err("the user connected, disconnected or shut down since the drop");
    }
    Ok(())
}

/// Everything the daemon remembers in order to reconnect.
///
/// Deliberately not `Debug`: it holds a password, and a stray `{:?}` is
/// exactly how one reaches a log file.
pub struct AutoReconnect {
    /// The last request that successfully connected. Captured on success
    /// only - retaining one that failed authentication is how a retry loop
    /// locks an account.
    request: Option<ConnectRequest>,
    enabled: bool,
    /// Sessions re-established without a human, this daemon's lifetime.
    pub reconnects: u32,
    /// Attempts during the current outage.
    pub attempts: u32,
    pub last_attempt_error: Option<String>,
    pub outage_began: Option<SystemTime>,
    pub next_attempt_at: Option<SystemTime>,
    /// Set when the loop will not try again by itself, and why.
    pub stopped_reason: Option<String>,
    /// Reconnects that did not last `HEALTHY_SESSION`.
    pub short_sessions: u32,
    /// When the last reconnect succeeded, to judge the next outage against.
    pub last_success: Option<SystemTime>,
    /// An attempt is running right now.
    ///
    /// A `connect` can take minutes (the automation bootstrap), and the
    /// frame processor of a session that dies during one sends its own drop
    /// event. Without this, that event spawns a second loop which races the
    /// first for the session slot.
    pub in_flight: bool,
}

impl Default for AutoReconnect {
    fn default() -> Self {
        Self {
            request: None,
            enabled: false,
            reconnects: 0,
            attempts: 0,
            last_attempt_error: None,
            outage_began: None,
            next_attempt_at: None,
            stopped_reason: None,
            short_sessions: 0,
            last_success: None,
            in_flight: false,
        }
    }
}

impl AutoReconnect {
    /// Remember a request that connected. Called on the success path only.
    pub fn remember(&mut self, request: ConnectRequest) {
        self.enabled = request.auto_reconnect;
        self.request = Some(request);
        // A fresh user connect clears a previous give-up: the operator has
        // evidently fixed whatever it was.
        self.stopped_reason = None;
        self.attempts = 0;
        self.short_sessions = 0;
        self.last_success = None;
        self.outage_began = None;
        self.next_attempt_at = None;
        // A connect that got this far means no attempt is meaningfully in
        // flight any more, and this is the one place that can unwedge the
        // flag if a task holding it ever died without clearing it.
        self.in_flight = false;
    }

    /// The retained request, for an attempt.
    pub fn request(&self) -> Option<ConnectRequest> {
        self.request.clone()
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Stop trying, permanently, until a human connects again.
    pub fn stop(&mut self, why: String) {
        self.next_attempt_at = None;
        self.stopped_reason = Some(why);
    }

    /// The loop is no longer going to act. Clears the schedule it published.
    ///
    /// Without this, `session info` keeps reporting a "next attempt" that is
    /// in the past and will never happen, which reads as "still trying".
    ///
    /// Deliberately does **not** touch `in_flight`: an attempt clears its
    /// own flag when it finishes, and a second loop standing down *because*
    /// one is already running would otherwise clear the running one's, which
    /// is the opposite of what it was checking for. Callers must not call
    /// this when the reason they are standing down is that flag.
    pub fn stand_down(&mut self) {
        self.next_attempt_at = None;
    }

    /// Note a new outage and say whether reconnecting is still worth it.
    ///
    /// A host that accepts the connection and drops it again immediately
    /// would otherwise be reconnected to all night, and every cycle can cost
    /// a Win+R on the desktop. Backoff does not help: each attempt
    /// "succeeds", so the attempt counter keeps resetting.
    pub fn note_outage(&mut self, now: SystemTime) -> bool {
        let short = self
            .last_success
            .and_then(|at| now.duration_since(at).ok())
            .is_some_and(|lived| lived < HEALTHY_SESSION);
        if short {
            self.short_sessions = self.short_sessions.saturating_add(1);
            if self.short_sessions >= MAX_SHORT_SESSIONS {
                self.stop(format!(
                    "the session was re-established {} times and dropped again within {}s each \
                     time; something other than a transient outage is ending it",
                    self.short_sessions,
                    HEALTHY_SESSION.as_secs()
                ));
                return false;
            }
        } else {
            self.short_sessions = 0;
        }
        true
    }

    /// Record a successful reconnect.
    pub fn note_success(&mut self, now: SystemTime) {
        self.reconnects = self.reconnects.saturating_add(1);
        self.attempts = 0;
        self.next_attempt_at = None;
        self.outage_began = None;
        self.last_attempt_error = None;
        self.last_success = Some(now);
    }

    /// Forget the credentials, on an explicit disconnect.
    pub fn disarm(&mut self) {
        self.enabled = false;
        self.request = None;
        self.next_attempt_at = None;
        self.outage_began = None;
        self.attempts = 0;
        // Deliberately not `in_flight`: an attempt that is mid-`connect`
        // clears it itself, and lying about it here would let a drop event
        // arriving during that attempt start a second loop.
    }

    /// What `session info` reports.
    pub fn to_protocol(&self) -> agent_rdp_protocol::AutoReconnectInfo {
        agent_rdp_protocol::AutoReconnectInfo {
            enabled: self.enabled,
            reconnects: self.reconnects,
            attempts: self.attempts,
            outage_began: self.outage_began.map(crate::timefmt::utc_rfc3339),
            next_attempt_at: self.next_attempt_at.map(crate::timefmt::utc_rfc3339),
            last_attempt_error: self.last_attempt_error.clone(),
            stopped_reason: self.stopped_reason.clone(),
        }
    }
}

/// Shared handle. A tokio mutex because the reconnect loop holds it across
/// awaits; the accept loop must never lock it inline.
pub type SharedAutoReconnect = Arc<tokio::sync::Mutex<AutoReconnect>>;

/// What to journal about an attempt, with nothing that could carry a secret.
///
/// `transcript::append_event` writes what it is given verbatim, and
/// `agent-rdp diagnose` puts that file in a zip for a bug report. Serialising
/// the retained `ConnectRequest` here would put the password in it.
pub fn attempt_event(
    request: &ConnectRequest,
    attempt: u32,
    outcome: &str,
    detail: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "rdp_auto_reconnect": {
            "host": request.host,
            "port": request.port,
            "username": request.username,
            "attempt": attempt,
            "outcome": outcome,
            "detail": detail,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn armed() -> ReconnectSnapshot {
        ReconnectSnapshot {
            enabled: true,
            have_request: true,
            stopped: false,
            in_flight: false,
            dropped_generation: 7,
            current_generation: 7,
            serial_at_arm: 3,
            serial_now: 3,
        }
    }

    #[test]
    fn an_armed_session_reconnects() {
        assert_eq!(should_attempt(&armed()), Ok(()));
    }

    /// The race the graceful-shutdown flag cannot catch: `disconnect` is
    /// issued against an already-sick link, the read arm errors first, and
    /// the processor exits by the failure path - sending a drop event for a
    /// session the user has just closed.
    #[test]
    fn a_user_disconnect_disarms_auto_reconnect() {
        let mut s = armed();
        s.serial_now = s.serial_at_arm + 1;
        assert!(should_attempt(&s).is_err());
        assert!(should_attempt(&s).unwrap_err().contains("disconnected"));
    }

    #[test]
    fn a_newer_session_cancels_the_reconnect() {
        let mut s = armed();
        s.current_generation = s.dropped_generation + 1;
        assert!(should_attempt(&s).is_err());
    }

    #[test]
    fn nothing_happens_without_the_flag_or_a_request() {
        let mut off = armed();
        off.enabled = false;
        assert!(should_attempt(&off).is_err());

        let mut empty = armed();
        empty.have_request = false;
        assert!(should_attempt(&empty).is_err());
    }

    #[test]
    fn a_permanent_stop_is_permanent() {
        let mut s = armed();
        s.stopped = true;
        assert!(should_attempt(&s).unwrap_err().contains("gave up"));
    }

    #[test]
    fn two_attempts_never_run_at_once() {
        let mut s = armed();
        s.in_flight = true;
        assert!(should_attempt(&s).is_err());
    }

    /// The guard above is only worth anything if the loop actually sets the
    /// flag it reads. It was passed a hardcoded `false` at both snapshot
    /// sites, so the test above proved nothing about production.
    #[test]
    fn the_loop_wires_in_flight_to_real_state() {
        let source = crate::automation::lf(include_str!("daemon.rs"));
        let at = source.find("async fn reconnect_loop(").expect("the loop");
        let body = &source[at..];
        let end = body.find("\n/// Process a single request").unwrap_or(body.len());
        let body = &body[..end];
        assert!(
            !body.contains("in_flight: false"),
            "the snapshot must read the flag, not assume it"
        );
        assert!(
            body.contains("in_flight = true"),
            "and the attempt must set it"
        );
    }

    /// The defect that made the whole feature one-shot. `connect::handle`
    /// bumps the session generation before it does anything else, so after
    /// our own failed attempt the counter is one ahead of the outage's - and
    /// comparing the original value against it concluded we had been
    /// superseded by ourselves, ending the loop for the night.
    #[test]
    fn the_loop_rebaselines_the_generation_after_its_own_attempt() {
        let source = crate::automation::lf(include_str!("daemon.rs"));
        let at = source.find("async fn reconnect_loop(").expect("the loop");
        let body = &source[at..];
        let end = body.find("\n/// Process a single request").unwrap_or(body.len());
        let body = &body[..end];
        assert!(
            body.contains("let mut dropped_generation = dropped_generation;"),
            "the baseline has to be able to move"
        );
        let handle_at = body.find("handlers::connect::handle(").expect("the connect call");
        assert!(
            body[handle_at..].contains("dropped_generation = ctx"),
            "and it has to move after the attempt, not before"
        );
    }

    /// A loop that has decided not to act must not leave a "next attempt"
    /// behind it. `session info` reads that field, and one stuck in the past
    /// reads as "still trying" forever.
    #[test]
    fn giving_up_clears_the_published_schedule() {
        let mut auto = AutoReconnect::default();
        auto.next_attempt_at = Some(SystemTime::now() + Duration::from_secs(30));
        auto.in_flight = true;
        auto.stand_down();
        assert!(auto.next_attempt_at.is_none());
        assert!(
            auto.in_flight,
            "an attempt owns its own flag; standing down must not clear someone else's"
        );
    }

    /// Long enough to ride out a laptop waking up, short enough that an
    /// overnight outage is not spent asleep.
    #[test]
    fn backoff_grows_and_then_caps() {
        assert_eq!(backoff(1), Duration::from_secs(5));
        assert_eq!(backoff(2), Duration::from_secs(10));
        assert_eq!(backoff(3), Duration::from_secs(20));
        assert_eq!(backoff(4), Duration::from_secs(40));
        assert_eq!(backoff(5), Duration::from_secs(60));
        assert_eq!(backoff(500), Duration::from_secs(60), "capped, not growing");
    }

    /// The retained request is the one that worked. Keeping a failed one is
    /// how an automatic retry locks a domain account overnight.
    #[test]
    fn only_a_working_request_is_retained() {
        let mut auto = AutoReconnect::default();
        assert!(auto.request().is_none());
        assert!(!auto.enabled());

        let request = ConnectRequest {
            host: "h".into(),
            username: "u".into(),
            password: "pw".into(),
            auto_reconnect: true,
            ..ConnectRequest::default()
        };
        auto.remember(request);
        assert!(auto.enabled());
        assert_eq!(auto.request().unwrap().host, "h");

        // A fresh connect clears an earlier give-up: the operator has
        // evidently dealt with whatever stopped it.
        auto.stop("credentials refused".into());
        assert!(auto.stopped_reason.is_some());
        auto.remember(ConnectRequest {
            password: "pw".into(),
            auto_reconnect: true,
            ..ConnectRequest::default()
        });
        assert!(auto.stopped_reason.is_none());

        auto.disarm();
        assert!(auto.request().is_none(), "an explicit disconnect forgets the credentials");
        assert!(!auto.enabled());
    }

    /// A host that accepts the connection and drops it straight back is not
    /// a transient outage, and no amount of backoff helps: each attempt
    /// "succeeds", so the attempt counter resets every time.
    #[test]
    fn a_session_that_keeps_dying_immediately_stops_the_loop() {
        let mut auto = AutoReconnect::default();
        auto.remember(ConnectRequest {
            password: "pw".into(),
            auto_reconnect: true,
            ..ConnectRequest::default()
        });

        let mut now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        for round in 1..MAX_SHORT_SESSIONS {
            auto.note_success(now);
            now += Duration::from_secs(10);
            assert!(auto.note_outage(now), "round {round} should still try");
            assert!(auto.stopped_reason.is_none());
        }
        auto.note_success(now);
        now += Duration::from_secs(10);
        assert!(!auto.note_outage(now), "the pattern is established");
        let why = auto.stopped_reason.expect("it says why it stopped");
        assert!(why.contains("dropped again within"), "{why}");

        // A session that lasted is not part of a pattern.
        let mut healthy = AutoReconnect::default();
        healthy.remember(ConnectRequest {
            password: "pw".into(),
            auto_reconnect: true,
            ..ConnectRequest::default()
        });
        let mut t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        for _ in 0..20 {
            healthy.note_success(t);
            t += HEALTHY_SESSION + Duration::from_secs(1);
            assert!(healthy.note_outage(t));
        }
        assert!(healthy.stopped_reason.is_none(), "an hourly drop is what this is for");
        assert_eq!(healthy.short_sessions, 0);
    }

    /// The guard against a later simplification typing Win+R at 03:00.
    ///
    /// `connect`'s own bootstrap has no idle gate - only the supervisor's
    /// retry does - so an automatic reconnect must adopt a survivor and
    /// otherwise hand the launch to the supervisor, never call
    /// `launch_guarded` itself.
    #[test]
    fn an_auto_reconnect_never_launches_the_agent_itself() {
        let source = crate::automation::lf(include_str!("handlers/connect.rs"));
        let at = source
            .find("} else if origin == ConnectOrigin::AutoReconnect {")
            .expect("the automatic-reconnect branch exists");
        let branch = &source[at..];
        let end = branch.find("\n        } else {").expect("the branch ends");
        let branch = &branch[..end];

        assert!(branch.contains("adopt_only"), "it must try to adopt first");
        assert!(
            !branch.contains("launch_guarded"),
            "an unattended reconnect must not type Win+R itself"
        );
        // Deferring would stop the supervisor bringing the agent back at
        // all, which is the opposite of what an unattended session needs.
        assert!(
            !branch.contains("launch_deferred"),
            "the supervisor must stay free to relaunch"
        );
        assert!(branch.contains("next_retry_at"), "and must be armed to do it");
    }

    /// The journal goes into `diagnose` zips. It must carry enough to debug
    /// an overnight outage and nothing that could be a secret.
    #[test]
    fn the_reconnect_journal_never_contains_the_password() {
        let request = ConnectRequest {
            host: "host.example".into(),
            username: "operator".into(),
            password: "hunter2".into(),
            domain: Some("CORP".into()),
            ..ConnectRequest::default()
        };
        let event = attempt_event(&request, 3, "failed", Some("connection refused"));
        let text = serde_json::to_string(&event).unwrap();

        assert!(!text.contains("hunter2"), "the password must never be journalled: {text}");
        assert!(text.contains("host.example"), "but the host must, or it cannot be debugged");
        assert!(text.contains("operator"));
        assert!(text.contains("connection refused"));
    }
}
