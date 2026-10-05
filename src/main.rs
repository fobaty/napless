mod app;
mod energy;
mod metrics;
mod power;
mod powermetrics;
mod session;

use std::env;
use std::process::exit;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use session::Session;

const USAGE: &str = "\
Napless - keep the MacBook running with the lid closed while on AC power

Usage: sudo napless [OPTIONS]

Options:
      --daemon                Run without a window, print status to the terminal
      --no-auto-start          Do not activate lid-closed mode on launch
      --display-timeout <MIN>  Display sleep timer for the session (0 = keep the
                               screen on). Defaults to the current timer, or 5 min
                               when the current one is disabled
  -h, --help                   Show this help

While active: sudo pmset -c sleep 0 disablesleep 1 displaysleep <MIN>
On exit (Quit, Ctrl+C, SIGTERM, SIGHUP) the previous values are restored.";

struct Options {
    daemon: bool,
    auto_start: bool,
    display_timeout: Option<u32>,
}

impl Options {
    fn parse() -> Self {
        let mut options = Self {
            daemon: false,
            auto_start: true,
            display_timeout: None,
        };
        let mut args = env::args().skip(1);

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => {
                    println!("{USAGE}");
                    exit(0);
                }
                "--daemon" => options.daemon = true,
                "--no-auto-start" => options.auto_start = false,
                "--display-timeout" => {
                    let raw = args.next().unwrap_or_default();
                    match raw.parse::<u32>() {
                        Ok(value) if value <= power::MAX_TIMEOUT => {
                            options.display_timeout = Some(value);
                        }
                        _ => {
                            eprintln!(
                                "Error: --display-timeout expects an integer 0..={} (got \"{raw}\")",
                                power::MAX_TIMEOUT
                            );
                            exit(1);
                        }
                    }
                }
                other => {
                    eprintln!("Error: unknown argument \"{other}\". Run: sudo napless --help");
                    exit(1);
                }
            }
        }

        options
    }
}

fn main() {
    let options = Options::parse();

    if unsafe { libc::geteuid() } != 0 {
        eprintln!("Error: administrator privileges are required. Try: sudo ./napless");
        exit(1);
    }

    let session = Arc::new(Session::new());

    if let Err(e) = ctrlc::set_handler({
        let session = Arc::clone(&session);
        move || session.request_shutdown()
    }) {
        eprintln!("Warning: could not install the signal handler: {e}");
    }

    if options.auto_start {
        match session.activate(options.display_timeout) {
            Ok(()) => announce(&session, options.daemon),
            Err(e) if options.daemon => {
                eprintln!("Error: {e}");
                exit(1);
            }
            Err(e) => eprintln!("Warning: could not activate lid-closed mode: {e}"),
        }
    }

    if options.daemon {
        run_daemon(&session);
        if let Err(e) = session.deactivate() {
            eprintln!("Warning: could not restore the power settings: {e}");
        }
    } else if let Err(e) = app::run(Arc::clone(&session)) {
        if let Err(restore) = session.deactivate() {
            eprintln!("Warning: could not restore the power settings: {restore}");
        }
        eprintln!("Error: {e}");
        exit(1);
    }
}

fn announce(session: &Session, daemon: bool) {
    if !daemon {
        return;
    }

    let status = session.status();
    println!("Lid-closed mode is active. You can close the lid now. Press Ctrl+C to stop.");
    println!("{}", status.note);
    if status.on_battery_power {
        println!(
            "Warning: running on battery power, the mode stays inactive until AC is plugged in."
        );
    }
}

fn run_daemon(session: &Session) {
    let mut cpu = metrics::CpuSampler::default();
    let mut system = powermetrics::SystemEnergy::start();
    let mut reported = false;

    while !session.shutdown_requested() {
        thread::sleep(Duration::from_secs(1));

        let status = session.status();
        if status.state != session::PowerState::Active {
            if reported {
                println!("{}", status.note);
                reported = false;
            }
            continue;
        }

        let memory = metrics::memory().map(|m| format!("{:.1}/{:.1} GB", m.used_gb, m.total_gb));
        let reading = system.poll();
        let system_line = match (reading.watts, &reading.error) {
            (Some(watts), None) => format!("{watts:.1} W  |  spent {}", reading.total),
            (_, Some(reason)) => format!("unavailable ({reason})"),
            _ => "reading\u{2026}".to_string(),
        };
        let power = energy::energy().map(|e| e.summary());
        println!(
            "active for {}  |  cpu {:>3}%  |  ram {}  |  battery {}  |  system {}  |  on battery: {}",
            session::format_elapsed(status.elapsed),
            cpu.sample()
                .map(|percent| format!("{percent:.0}"))
                .unwrap_or_else(|| "--".to_string()),
            memory.unwrap_or_else(|| "--".to_string()),
            power.unwrap_or_else(|| "--".to_string()),
            system_line,
            status.on_battery_power,
        );
        reported = true;
    }
}
