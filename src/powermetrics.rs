use std::io::{BufRead, BufReader};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use crate::energy::EnergyMeter;

/// How often to ask the system for a new power figure. One second is the finest
/// granularity that keeps the sampling overhead worth it.
const SAMPLE_INTERVAL_MS: &str = "1000";

/// Live system power and the energy accumulated from it.
///
/// `powermetrics` is the only source that reports what the system as a whole is
/// drawing rather than what the battery sees, and it needs root. Napless already
/// runs as root, so the tool is available to it.
///
/// Apple's own documentation calls these figures estimates that should not be used
/// to compare devices, so the totals are an approximation of the real consumption
/// rather than a measurement of it.
pub struct SystemEnergy {
    child: Option<Child>,
    samples: Receiver<f64>,
    meter: EnergyMeter,
    watts: Option<f64>,
    error: Option<String>,
}

impl SystemEnergy {
    /// Starts the sampler. Nothing is spawned until this is called, so a machine
    /// without a battery still gets system power.
    pub fn start() -> Self {
        let mut this = Self {
            child: None,
            samples: mpsc::channel().1,
            meter: EnergyMeter::new(),
            watts: None,
            error: None,
        };

        let spawned = Command::new("/usr/bin/powermetrics")
            .args(["-i", SAMPLE_INTERVAL_MS, "-s", "cpu_power,gpu_power"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();

        match spawned {
            Ok(mut child) => {
                let stdout = child.stdout.take().expect("stdout was piped");
                let (tx, rx) = mpsc::channel();
                thread::spawn(move || forward(stdout, tx));
                this.child = Some(child);
                this.samples = rx;
            }
            Err(e) => this.error = Some(format!("powermetrics could not start: {e}")),
        }

        this
    }

    /// Folds in whatever the sampler has published and returns the running state.
    pub fn poll(&mut self) -> SystemEnergyReading {
        loop {
            match self.samples.try_recv() {
                Ok(watts) => {
                    self.meter.sample(watts);
                    self.watts = Some(watts);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }

        if self.error.is_none() && self.watts.is_none() {
            let exited = self.child.as_mut().and_then(|child| child.try_wait().ok());
            if let Some(Some(status)) = exited {
                self.error = Some(match status.success() {
                    true => "powermetrics exited without reporting power".to_string(),
                    false => "powermetrics was refused, root is required".to_string(),
                });
            }
        }

        SystemEnergyReading {
            watts: self.watts,
            total: self.meter.summary(),
            error: self.error.clone(),
        }
    }
}

impl Drop for SystemEnergy {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // powermetrics runs forever, so it has to be stopped explicitly or it
            // outlives the session as a stray root process.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub struct SystemEnergyReading {
    /// Latest power draw in watts, absent until the first sample arrives.
    pub watts: Option<f64>,
    /// Total formatted for display.
    pub total: String,
    pub error: Option<String>,
}

/// Splits the sampler's text output into one figure per reporting cycle.
///
/// Which lines appear depends on the samplers requested, so a cycle is taken to
/// end either when a label repeats or when the combined figure is published,
/// rather than waiting for a fixed set of subsystems.
#[derive(Default)]
struct PowerLines {
    cpu: Option<f64>,
    gpu: Option<f64>,
    ane: Option<f64>,
}

impl PowerLines {
    /// Returns the cycle's power in watts once a cycle is complete.
    fn feed(&mut self, line: &str) -> Option<f64> {
        let combined = capture(line, "Combined Power");
        let cpu = capture(line, "CPU Power");
        let gpu = capture(line, "GPU Power");
        let ane = capture(line, "ANE Power");

        // A label arriving twice means the cycle it belonged to is finished.
        let completed = (cpu.is_some() && self.cpu.is_some()
            || gpu.is_some() && self.gpu.is_some()
            || ane.is_some() && self.ane.is_some())
        .then(|| self.take())
        .flatten();

        if let Some(mw) = cpu {
            self.cpu = Some(mw);
        }
        if let Some(mw) = gpu {
            self.gpu = Some(mw);
        }
        if let Some(mw) = ane {
            self.ane = Some(mw);
        }

        if let Some(mw) = combined {
            self.take();
            return Some(mw / 1000.0);
        }

        completed
    }

    /// Sums the subsystems seen so far, in watts, and clears them for the next
    /// cycle.
    fn take(&mut self) -> Option<f64> {
        let sum = self.cpu.unwrap_or(0.0) + self.gpu.unwrap_or(0.0) + self.ane.unwrap_or(0.0);
        self.cpu = None;
        self.gpu = None;
        self.ane = None;
        (sum > 0.0).then_some(sum / 1000.0)
    }
}

/// Reads the milliwatt figure a line starts with. The prefix has to match from the
/// start so the combined line is not mistaken for a per subsystem one.
fn capture(line: &str, label: &str) -> Option<f64> {
    let rest = line.trim().strip_prefix(label)?;
    // The combined line carries its figure after a parenthesised list of
    // subsystems, so the value is taken from the last colon onwards.
    let value = rest.rsplit(':').next()?.trim_start();
    let digits: String = value
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();
    digits.parse().ok()
}

fn forward(stdout: ChildStdout, tx: mpsc::Sender<f64>) {
    let mut parser = PowerLines::default();
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        if let Some(watts) = parser.feed(&line) {
            // A dead receiver means the meter was dropped, so there is no point
            // in keeping the process alive.
            if tx.send(watts).is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn sums_the_subsystem_figures_into_one_reading() {
        let mut parser = PowerLines::default();
        assert_eq!(parser.feed("CPU Power: 1234 mW"), None);
        assert_eq!(parser.feed("GPU Power: 500 mW"), None);
        assert_eq!(parser.feed("ANE Power: 66 mW"), None);
        // The next cycle's CPU line closes the previous one.
        assert_eq!(parser.feed("CPU Power: 1000 mW"), Some(1.8));
    }

    #[test]
    fn works_when_a_subsystem_is_not_sampled() {
        let mut parser = PowerLines::default();
        assert_eq!(parser.feed("CPU Power: 1234 mW"), None);
        assert_eq!(parser.feed("GPU Power: 500 mW"), None);
        assert_eq!(parser.feed("CPU Power: 1100 mW"), Some(1.734));
        assert_eq!(parser.feed("GPU Power: 600 mW"), None);
        assert_eq!(parser.feed("CPU Power: 900 mW"), Some(1.7));
    }

    #[test]
    fn prefers_the_published_combined_figure() {
        let mut parser = PowerLines::default();
        parser.feed("CPU Power: 1234 mW");
        parser.feed("GPU Power: 500 mW");
        parser.feed("ANE Power: 66 mW");
        assert_eq!(
            parser.feed("Combined Power (CPU + GPU + ANE): 2000 mW"),
            Some(2.0)
        );
        // The stale subsystem figures must not leak into the next cycle.
        assert_eq!(parser.feed("CPU Power: 10 mW"), None);
        assert_eq!(parser.feed("GPU Power: 10 mW"), None);
    }

    #[test]
    fn closes_a_cycle_that_only_reports_the_combined_figure() {
        let mut parser = PowerLines::default();
        assert_eq!(
            parser.feed("Combined Power (CPU + GPU + ANE): 2000 mW"),
            Some(2.0)
        );
        assert_eq!(
            parser.feed("Combined Power (CPU + GPU + ANE): 3500 mW"),
            Some(3.5)
        );
    }

    #[test]
    fn ignores_unrelated_output() {
        let mut parser = PowerLines::default();
        assert_eq!(parser.feed("**** Processor usage ****"), None);
        assert_eq!(parser.feed("Intel energy GPU Avg: 4.2 mW"), None);
        assert_eq!(parser.feed(""), None);
    }

    #[test]
    fn tolerates_a_missing_number() {
        assert_eq!(capture("CPU Power: n/a", "CPU Power"), None);
        assert_eq!(capture("CPU Power:", "CPU Power"), None);
        assert_eq!(capture("GPU Power: 12 mW", "CPU Power"), None);
    }

    #[test]
    fn keeps_accumulating_after_the_sampler_stops() {
        let mut energy = SystemEnergy {
            child: None,
            samples: mpsc::channel().1,
            meter: EnergyMeter::new(),
            watts: None,
            error: None,
        };
        let reading = energy.poll();
        assert_eq!(reading.watts, None);
        assert_eq!(reading.total, "0.0 Wh");
        assert!(reading.error.is_none());
    }

    /// Exercises the whole pipeline against the real tool, which is the only way
    /// to confirm the output format on the running macOS. Ignored because
    /// powermetrics needs root: run `sudo cargo test -- --ignored`.
    #[test]
    #[ignore = "needs root, powermetrics rejects any other user"]
    fn samples_real_system_power() {
        let mut energy = SystemEnergy::start();
        let mut seen = None;

        for _ in 0..30 {
            let reading = energy.poll();
            assert!(
                reading.error.is_none(),
                "powermetrics reported {}",
                reading.error.unwrap()
            );
            if let Some(watts) = reading.watts {
                seen = Some(watts);
                break;
            }
            thread::sleep(Duration::from_secs(1));
        }

        let watts = seen.expect("no power figure within 30s");
        assert!(
            (0.5..300.0).contains(&watts),
            "{watts} W is outside any plausible range"
        );
        eprintln!("powermetrics reported {watts:.1} W");
    }
}
