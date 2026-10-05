use std::process::{Command, Stdio};

/// Applied only when the machine is running with `displaysleep 0`, so that a
/// lid-closed session always ends up with the panel powered down.
pub const DEFAULT_DISPLAY_TIMEOUT: u32 = 5;
pub const MAX_TIMEOUT: u32 = 999;

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct PowerSettings {
    pub sleep: u32,
    pub displaysleep: u32,
    pub disablesleep: u32,
}

impl PowerSettings {
    /// The display timer that should be used for the session: the current one,
    /// unless it is disabled outright.
    pub fn session_display_timeout(self) -> u32 {
        match self.displaysleep {
            0 => DEFAULT_DISPLAY_TIMEOUT,
            v => v,
        }
    }

    /// System sleep and idle sleep are blocked, display sleep is kept intact:
    /// the Mac keeps running with the lid shut while the screen powers down.
    pub fn lid_closed_session(self, display_timeout: u32) -> Self {
        Self {
            sleep: 0,
            displaysleep: display_timeout,
            disablesleep: 1,
        }
    }
}

/// `pmset` exits with status 0 even when it refuses to act and only prints a
/// diagnostic, so success is defined by an empty stderr.
pub fn run_pmset(args: &[&str]) -> Result<String, String> {
    let output = Command::new("pmset")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("failed to run pmset: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if output.status.success() && stderr.is_empty() {
        return Ok(stdout);
    }

    let reason = if stderr.is_empty() {
        format!("exit code {:?}", output.status.code())
    } else {
        stderr
    };
    Err(format!("`pmset {}` failed: {reason}", args.join(" ")))
}

pub fn settings_args(settings: &PowerSettings) -> Vec<String> {
    let mut args = vec!["-c".to_string(), "sleep".to_string()];
    args.push(settings.sleep.to_string());
    for (key, value) in [
        ("displaysleep", settings.displaysleep),
        ("disablesleep", settings.disablesleep),
    ] {
        args.push(key.to_string());
        args.push(value.to_string());
    }
    args
}

pub fn apply(settings: &PowerSettings) -> Result<(), String> {
    let args = settings_args(settings);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run_pmset(&args).map(|_| ())
}

pub fn parse_ac_block(output: &str) -> PowerSettings {
    let mut settings = PowerSettings::default();
    let mut in_ac = false;

    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.ends_with(':') {
            in_ac = line.starts_with("AC Power");
            continue;
        }
        if !in_ac {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(key), Some(value)) = (parts.next(), parts.next()) else {
            continue;
        };
        let Ok(value) = value.parse::<u32>() else {
            continue;
        };
        match key {
            "sleep" => settings.sleep = value,
            "displaysleep" => settings.displaysleep = value,
            "disablesleep" => settings.disablesleep = value,
            _ => {}
        }
    }

    settings
}

pub fn ac_settings() -> Result<PowerSettings, String> {
    Ok(parse_ac_block(&run_pmset(&["-g", "custom"])?))
}

pub fn on_battery_power() -> bool {
    run_pmset(&["-g", "batt"])
        .map(|out| out.contains("Battery Power"))
        .unwrap_or(false)
}

/// Powers the panel down right away; any input or lid wake brings it back.
pub fn blank_display() -> Result<(), String> {
    run_pmset(&["displaysleepnow"]).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
Battery Power:
 lidwake              1
 sleep                5
 displaysleep         2
 disksleep            10
hibernatemode        3
AC Power:
 lidwake              1
 sleep                1
 displaysleep         10
 hibernatemode        3
 disksleep            10
";

    #[test]
    fn parses_ac_profile_and_ignores_battery() {
        assert_eq!(
            parse_ac_block(SAMPLE),
            PowerSettings {
                sleep: 1,
                displaysleep: 10,
                disablesleep: 0,
            }
        );
    }

    #[test]
    fn reads_existing_disablesleep() {
        let output = format!("{SAMPLE} disablesleep         1");
        assert_eq!(parse_ac_block(&output).disablesleep, 1);
    }

    #[test]
    fn missing_profile_yields_zero_defaults() {
        assert_eq!(parse_ac_block(""), PowerSettings::default());
    }

    #[test]
    fn builds_set_command_from_settings() {
        assert_eq!(
            settings_args(&PowerSettings {
                sleep: 7,
                displaysleep: 3,
                disablesleep: 1,
            }),
            ["-c", "sleep", "7", "displaysleep", "3", "disablesleep", "1"]
        );
    }

    #[test]
    fn session_blocks_sleep_but_keeps_display_timeout() {
        let original = PowerSettings {
            sleep: 1,
            displaysleep: 10,
            disablesleep: 0,
        };
        let session = original.lid_closed_session(original.session_display_timeout());
        assert_eq!(session.disablesleep, 1);
        assert_eq!(session.sleep, 0);
        assert_eq!(session.displaysleep, 10);
    }

    #[test]
    fn replaces_never_display_timer_with_default() {
        let original = PowerSettings {
            sleep: 1,
            displaysleep: 0,
            disablesleep: 0,
        };
        assert_eq!(original.session_display_timeout(), DEFAULT_DISPLAY_TIMEOUT);
    }
}
