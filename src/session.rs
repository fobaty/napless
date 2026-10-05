use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::power::{self, PowerSettings};

const BATTERY_POLL: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PowerState {
    Inactive,
    Active,
}

#[derive(Clone, Debug)]
pub struct Status {
    pub state: PowerState,
    pub elapsed: Duration,
    pub display_timeout: u32,
    pub on_battery_power: bool,
    pub note: String,
}

struct Inner {
    original: Option<PowerSettings>,
    started_at: Option<Instant>,
    display_timeout: u32,
    note: String,
    on_battery_power: bool,
    battery_checked_at: Option<Instant>,
}

/// Owns the power settings lifecycle: snapshot on activate, exact restore on
/// deactivate. Safe to call from both the UI thread and the signal handler.
pub struct Session {
    inner: Mutex<Inner>,
    shutdown: AtomicBool,
}

impl Session {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                original: None,
                started_at: None,
                display_timeout: power::DEFAULT_DISPLAY_TIMEOUT,
                note: "Not running. Activate to keep the Mac awake with the lid closed."
                    .to_string(),
                on_battery_power: false,
                battery_checked_at: None,
            }),
            shutdown: AtomicBool::new(false),
        }
    }

    pub fn is_active(&self) -> bool {
        self.inner().started_at.is_some()
    }

    /// The settings that will be put back on deactivate, if a session is live.
    pub fn snapshot(&self) -> Option<PowerSettings> {
        self.inner().original
    }

    pub fn activate(&self, display_timeout: Option<u32>) -> Result<(), String> {
        let mut inner = self.inner();

        if inner.started_at.is_some() {
            return Ok(());
        }

        let original = power::ac_settings()?;
        let timeout = display_timeout.unwrap_or_else(|| original.session_display_timeout());
        power::apply(&original.lid_closed_session(timeout))?;

        inner.original = Some(original);
        inner.started_at = Some(Instant::now());
        inner.display_timeout = timeout;
        inner.note = if timeout == 0 {
            "Running. The display will stay on.".to_string()
        } else {
            format!(
                "Running. The display powers off after {timeout} min; opening the lid wakes it."
            )
        };
        Ok(())
    }

    pub fn deactivate(&self) -> Result<(), String> {
        let mut inner = self.inner();

        let Some(original) = inner.original.take() else {
            inner.note =
                "Not running. Activate to keep the Mac awake with the lid closed.".to_string();
            return Ok(());
        };

        inner.started_at = None;
        let result = power::apply(&original);
        inner.note = match &result {
            Ok(()) => "Restored. The Mac can sleep normally again.".to_string(),
            Err(e) => format!("Restore failed: {e}"),
        };
        result
    }

    pub fn status(&self) -> Status {
        let mut inner = self.inner();
        self.refresh_battery(&mut inner);

        Status {
            state: if inner.started_at.is_some() {
                PowerState::Active
            } else {
                PowerState::Inactive
            },
            elapsed: inner.started_at.map(|at| at.elapsed()).unwrap_or_default(),
            display_timeout: inner.display_timeout,
            on_battery_power: inner.on_battery_power,
            note: inner.note.clone(),
        }
    }

    fn refresh_battery(&self, inner: &mut Inner) {
        let stale = inner
            .battery_checked_at
            .map(|at| at.elapsed() >= BATTERY_POLL)
            .unwrap_or(true);
        if stale {
            inner.on_battery_power = power::on_battery_power();
            inner.battery_checked_at = Some(Instant::now());
        }
    }

    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    pub fn shutdown_requested(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

pub fn format_elapsed(elapsed: Duration) -> String {
    let total = elapsed.as_secs();
    format!(
        "{:02}:{:02}:{:02}",
        total / 3600,
        (total % 3600) / 60,
        total % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_elapsed_time_with_hours() {
        assert_eq!(format_elapsed(Duration::from_secs(0)), "00:00:00");
        assert_eq!(format_elapsed(Duration::from_secs(59)), "00:00:59");
        assert_eq!(format_elapsed(Duration::from_secs(61)), "00:01:01");
        assert_eq!(format_elapsed(Duration::from_secs(3_661)), "01:01:01");
        assert_eq!(format_elapsed(Duration::from_secs(90_061)), "25:01:01");
    }

    #[test]
    fn fresh_session_is_inactive() {
        let session = Session::new();
        assert!(!session.is_active());
        assert_eq!(session.status().state, PowerState::Inactive);
        assert_eq!(session.status().elapsed, Duration::ZERO);
    }

    #[test]
    fn deactivate_without_activate_is_a_no_op() {
        let session = Session::new();
        assert!(session.deactivate().is_ok());
        assert!(session.snapshot().is_none());
        assert!(!session.is_active());
    }

    #[test]
    fn shutdown_flag_is_visible_to_the_ui() {
        let session = Session::new();
        assert!(!session.shutdown_requested());
        session.request_shutdown();
        assert!(session.shutdown_requested());
    }
}
