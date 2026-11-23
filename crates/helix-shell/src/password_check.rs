//! The lock screen's password check, as a state: which check is running,
//! and what to make of a result or a timeout (helixdesktop/desktop#244).
//!
//! PAM runs on a thread that can't be stopped, and some setups (sssd,
//! LDAP, a fingerprint reader's timeout) can keep it for a long time. The
//! lock screen ignores keys while a check runs, so after [`TIMEOUT`] it
//! takes input again; the stuck check's answer, if it ever comes, is used
//! only when nothing newer was tried.

use std::time::Duration;

/// How long a check may run before the lock screen takes input again.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// What to do with a check's result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Show it (unlock, or say the password was wrong).
    Apply,
    /// Drop it: a newer check was started, or it came too late to mean
    /// anything but a success.
    Ignore,
}

/// The checks of one lock screen.
#[derive(Debug, Default)]
pub struct Checks {
    /// The latest check started.
    latest: u64,
    /// The check running now, if any.
    running: Option<u64>,
}

impl Checks {
    /// A check starts: its serial (results and timeouts carry it).
    pub fn start(&mut self) -> u64 {
        self.latest += 1;
        self.running = Some(self.latest);
        self.latest
    }

    pub fn running(&self) -> bool {
        self.running.is_some()
    }

    /// `serial`'s time is up: whether it was still running (the lock
    /// screen then takes input again).
    pub fn timed_out(&mut self, serial: u64) -> bool {
        if self.running == Some(serial) {
            self.running = None;
            return true;
        }
        false
    }

    /// `serial`'s result came: `ok` is whether the password was right.
    pub fn result(&mut self, serial: u64, ok: bool) -> Outcome {
        if self.running == Some(serial) {
            self.running = None;
            return Outcome::Apply;
        }
        // After its timeout: a success still unlocks (the password was
        // right), unless the user has tried again since; a failure says
        // nothing new.
        if self.running.is_none() && serial == self.latest && ok {
            return Outcome::Apply;
        }
        Outcome::Ignore
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_result_in_time_applies() {
        let mut checks = Checks::default();
        let s = checks.start();
        assert!(checks.running());
        assert_eq!(checks.result(s, false), Outcome::Apply);
        assert!(!checks.running());
        assert!(!checks.timed_out(s), "already answered");
    }

    #[test]
    fn a_stuck_check_gives_input_back() {
        let mut checks = Checks::default();
        let s = checks.start();
        assert!(checks.timed_out(s));
        assert!(!checks.running(), "input is taken again");
        // Its late failure is dropped, its late success unlocks.
        assert_eq!(checks.result(s, false), Outcome::Ignore);
        assert_eq!(checks.result(s, true), Outcome::Apply);
    }

    #[test]
    fn an_older_check_never_applies() {
        let mut checks = Checks::default();
        let old = checks.start();
        assert!(checks.timed_out(old));
        let new = checks.start();
        assert_eq!(checks.result(old, true), Outcome::Ignore, "the user tried again since");
        assert!(checks.running(), "the newer check still runs");
        assert!(!checks.timed_out(old), "an older timeout doesn't end the newer check");
        assert_eq!(checks.result(new, false), Outcome::Apply);
    }
}
