use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::metrics::{self, CpuSampler, Memory, Temperature, TemperatureProbe};
use crate::power;
use crate::session::{self, PowerState, Session};

const SAMPLE_INTERVAL: Duration = Duration::from_millis(700);

struct Snapshot {
    cpu_percent: Option<f64>,
    memory: Option<Memory>,
    temperature: Temperature,
}

pub fn run(session: Arc<Session>) -> Result<(), String> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Napless")
            .with_inner_size([420.0, 330.0])
            .with_min_inner_size([400.0, 310.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Napless",
        options,
        Box::new(|_cc| Ok(Box::new(NaplessApp::new(session)))),
    )
    .map_err(|e| format!("could not start the window: {e}"))
}

struct NaplessApp {
    session: Arc<Session>,
    cpu: CpuSampler,
    probe: TemperatureProbe,
    snapshot: Snapshot,
    last_sample: Instant,
    error: Option<String>,
    closing: bool,
}

impl NaplessApp {
    fn new(session: Arc<Session>) -> Self {
        Self {
            session,
            cpu: CpuSampler::default(),
            probe: TemperatureProbe::start(),
            snapshot: Snapshot {
                cpu_percent: None,
                memory: None,
                temperature: Temperature::Pending,
            },
            last_sample: Instant::now(),
            error: None,
            closing: false,
        }
    }

    /// The timer is refreshed every frame so the elapsed time ticks smoothly,
    /// the gauges only need a slower cadence.
    fn refresh(&mut self, ctx: &egui::Context) {
        if self.session.is_active() {
            ctx.request_repaint_after(Duration::from_millis(200));
        }

        if self.last_sample.elapsed() < SAMPLE_INTERVAL {
            return;
        }
        self.last_sample = Instant::now();

        if let Some(percent) = self.cpu.sample() {
            self.snapshot.cpu_percent = Some(percent);
        }
        self.snapshot.memory = metrics::memory();
        self.snapshot.temperature = self.probe.poll();
    }

    fn header(&self, ui: &mut egui::Ui, status: &session::Status) {
        ui.horizontal(|ui| {
            let (color, label) = match status.state {
                PowerState::Active => (egui::Color32::from_rgb(46, 160, 67), "ACTIVE"),
                PowerState::Inactive => (egui::Color32::from_rgb(140, 140, 140), "INACTIVE"),
            };
            ui.colored_label(color, egui::RichText::new(label).strong());
            ui.label(
                egui::RichText::new(session::format_elapsed(status.elapsed))
                    .monospace()
                    .size(20.0),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.small(if status.on_battery_power {
                    "on battery"
                } else {
                    "on AC power"
                });
            });
        });

        ui.small(
            egui::RichText::new(if status.state == PowerState::Active {
                format!(
                    "The screen powers off after {} min, opening the lid wakes it.",
                    status.display_timeout
                )
            } else {
                "System sleep is left untouched while inactive.".to_string()
            })
            .weak(),
        );
    }

    fn gauges(&self, ui: &mut egui::Ui) {
        let cpu_percent = self.snapshot.cpu_percent.unwrap_or_default();
        let cpu_label = match self.snapshot.cpu_percent {
            Some(percent) => format!("CPU   {percent:.0}%"),
            None => "CPU   --".to_string(),
        };
        ui.add(bar(
            cpu_percent as f32 / 100.0,
            cpu_label,
            cpu_percent as f32,
        ));
        ui.add_space(4.0);

        let memory_label = match self.snapshot.memory {
            Some(memory) => format!(
                "RAM   {:.1} / {:.1} GB   {:.0}%",
                memory.used_gb, memory.total_gb, memory.percent
            ),
            None => "RAM   --".to_string(),
        };
        let memory_percent = self.snapshot.memory.map(|m| m.percent).unwrap_or_default();
        ui.add(bar(
            memory_percent as f32 / 100.0,
            memory_label,
            memory_percent as f32,
        ));
        ui.add_space(4.0);

        let temperature = match &self.snapshot.temperature {
            Temperature::Value(value) => {
                let sensor = self
                    .probe
                    .sensor_name()
                    .map(|name| format!(" [{name}]"))
                    .unwrap_or_default();
                egui::RichText::new(format!("CPU temperature   {value:.0}\u{b0}C{sensor}"))
                    .monospace()
            }
            Temperature::Pending => egui::RichText::new("CPU temperature   reading\u{2026}").weak(),
            Temperature::Unavailable(reason) => {
                egui::RichText::new(format!("CPU temperature   unavailable ({reason})")).weak()
            }
        };
        ui.label(temperature);
    }

    fn buttons(&mut self, ui: &mut egui::Ui) {
        let active = self.session.is_active();

        ui.horizontal(|ui| {
            if active {
                if ui
                    .add_sized([140.0, 28.0], egui::Button::new("Deactivate"))
                    .clicked()
                {
                    self.error = self.session.deactivate().err();
                }
            } else if ui
                .add_sized([140.0, 28.0], egui::Button::new("Activate"))
                .clicked()
            {
                self.error = self.session.activate(None).err();
            }

            if ui
                .add_sized([130.0, 28.0], egui::Button::new("Blank Screen"))
                .clicked()
            {
                self.error = power::blank_display().err();
            }
        });

        ui.add_space(6.0);
        ui.small(
            egui::RichText::new(
                "Lid-closed mode covers AC power only: closing the lid keeps the Mac running, \
                 opening it wakes the screen.",
            )
            .weak(),
        );
    }

    fn footer(&mut self, ui: &mut egui::Ui, status: &session::Status) {
        if let Some(error) = &self.error {
            ui.colored_label(egui::Color32::from_rgb(200, 60, 60), error);
        } else {
            ui.small(status.note.clone());
        }

        if let Some(original) = self.session.snapshot() {
            ui.small(
                egui::RichText::new(format!(
                    "On quit: restores sleep={}, displaysleep={}, disablesleep={}.",
                    original.sleep, original.displaysleep, original.disablesleep
                ))
                .weak(),
            );
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Quit").clicked() {
                self.closing = true;
            }
        });
    }
}

fn bar(fraction: f32, text: String, percent: f32) -> egui::ProgressBar {
    egui::ProgressBar::new(fraction.clamp(0.0, 1.0))
        .text(text)
        .desired_height(18.0)
        .fill(gauge_color(percent))
}

fn gauge_color(percent: f32) -> egui::Color32 {
    match percent {
        p if p >= 90.0 => egui::Color32::from_rgb(200, 60, 60),
        p if p >= 70.0 => egui::Color32::from_rgb(214, 154, 33),
        _ => egui::Color32::from_rgb(66, 133, 200),
    }
}

impl eframe::App for NaplessApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.refresh(&ctx);
        if self.session.shutdown_requested() {
            self.closing = true;
        }

        egui::Frame::central_panel(ui.style()).show(ui, |ui| {
            let status = self.session.status();
            self.header(ui, &status);
            ui.separator();
            ui.add_space(4.0);
            self.gauges(ui);
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(4.0);
            self.buttons(ui);
            ui.separator();
            self.footer(ui, &status);
        });

        if self.closing {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// Covers window close, Cmd+Q and any other orderly shutdown of eframe.
    fn on_exit(&mut self) {
        if let Err(e) = self.session.deactivate() {
            eprintln!("Warning: could not restore the power settings: {e}");
        }
    }
}
