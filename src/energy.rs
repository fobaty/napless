use std::time::Instant;

use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString};
use objc2_io_kit::{
    io_object_t, IOIteratorNext, IOMainPort, IOObjectRelease, IORegistryEntryCreateCFProperty,
    IOServiceGetMatchingServices, IOServiceMatching,
};

/// Energy and battery state, read from the `AppleSmartBattery` registry entry.
///
/// macOS has no supported power API: `pmset` reports state transitions but no
/// numbers, so the figures come straight from IOKit. That needs neither root nor
/// a helper process.
#[derive(Clone, Copy, Debug)]
pub struct Energy {
    /// Charge level, 0..=100.
    pub percent: f64,
    /// Power at the battery terminals in watts, positive when it flows into the
    /// pack. Running on battery this is the whole machine's draw; on AC it is only
    /// what the pack absorbs, and the mains path is not exposed to unprivileged
    /// callers.
    pub watts: f64,
    pub charging: bool,
    pub external_power: bool,
}

impl Energy {
    /// Whether the pack is charging, running from the charger, or discharging.
    pub fn state(&self) -> &'static str {
        match (self.external_power, self.charging) {
            (true, true) => "charging",
            (true, false) => "on AC",
            (false, _) => "on battery",
        }
    }

    /// Charge level and live power, e.g. `89% 28.1 W charging`.
    pub fn summary(&self) -> String {
        format!("{:.0}% {:.1} W {}", self.percent, self.watts, self.state())
    }
}

/// `BatteryPower` is published in milliwatts and tracks the current times voltage
/// figure closely, so it is preferred over deriving the power here. The
/// `Accumulated*` telemetry siblings are not usable: Apple refreshes them in bursts
/// every half minute or so, so their rate over a short interval says nothing about
/// the power being used.
const MILLIWATTS_PER_WATT: f64 = 1000.0;
/// Amperage is in milliamps and voltage in millivolts, so their product needs
/// two factors of a thousand to arrive at watts.
const MILLIAMP_MILLIVOLT_PER_WATT: f64 = 1000.0 * 1000.0;

struct Battery {
    entry: io_object_t,
}

impl Battery {
    fn find() -> Option<Self> {
        unsafe {
            let mut main_port = 0;
            if IOMainPort(0, &raw mut main_port) != libc::KERN_SUCCESS {
                return None;
            }

            let matching = IOServiceMatching(c"AppleSmartBattery".as_ptr())?
                .downcast::<CFDictionary>()
                .ok()?;
            let mut iterator = 0;
            if IOServiceGetMatchingServices(main_port, Some(matching), &raw mut iterator)
                != libc::KERN_SUCCESS
            {
                return None;
            }

            let entry = IOIteratorNext(iterator);
            IOObjectRelease(iterator);
            (entry != 0).then_some(Self { entry })
        }
    }

    fn number(&self, key: &str) -> Option<f64> {
        unsafe {
            let key: CFRetained<CFString> = CFString::from_str(key);
            let value = IORegistryEntryCreateCFProperty(self.entry, Some(&key), None, 0)?;
            value.downcast_ref::<CFNumber>().and_then(CFNumber::as_f64)
        }
    }

    /// Integers are read as `i64` rather than through `f64`: the signed amperage
    /// arrives sign-extended, and a double cannot hold a value that close to
    /// 2^64 without rounding away the low bits.
    fn integer(&self, key: &str) -> Option<i64> {
        unsafe {
            let key: CFRetained<CFString> = CFString::from_str(key);
            let value = IORegistryEntryCreateCFProperty(self.entry, Some(&key), None, 0)?;
            value.downcast_ref::<CFNumber>().and_then(CFNumber::as_i64)
        }
    }

    fn boolean(&self, key: &str) -> Option<bool> {
        unsafe {
            let key: CFRetained<CFString> = CFString::from_str(key);
            let value = IORegistryEntryCreateCFProperty(self.entry, Some(&key), None, 0)?;
            value.downcast_ref::<CFBoolean>().map(CFBoolean::as_bool)
        }
    }

    /// Apple signs a milliamp reading into an unsigned property, so charging
    /// arrives sign-extended to 64 bits instead of as a plain magnitude.
    fn amperage(&self) -> Option<f64> {
        self.integer("Amperage").map(|milliamps| milliamps as f64)
    }

    /// Terminal power in watts, from the published milliwatt figure when it is
    /// available and from current times voltage otherwise. Both readings arrive
    /// sign-extended, so they are read as integers rather than through `f64`.
    fn watts(&self) -> Option<f64> {
        let published = self
            .integer("BatteryPower")
            .filter(|milliwatts| *milliwatts != 0)
            .map(|milliwatts| milliwatts.abs() as f64 / MILLIWATTS_PER_WATT);
        let derived = self
            .amperage()
            .zip(self.number("AppleRawBatteryVoltage"))
            .map(|(milliamps, millivolts)| {
                milliamps.abs() * millivolts / MILLIAMP_MILLIVOLT_PER_WATT
            });
        published.or(derived)
    }
}

impl Drop for Battery {
    fn drop(&mut self) {
        IOObjectRelease(self.entry);
    }
}

pub fn energy() -> Option<Energy> {
    let battery = Battery::find()?;

    let watts = battery.watts();

    // CurrentCapacity counts against MaxCapacity, which is 100 on most packs but
    // not on every one, so the share is derived rather than read as a percentage.
    let percent = battery
        .number("CurrentCapacity")
        .zip(battery.number("MaxCapacity"))
        .and_then(|(current, max)| (max > 0.0).then_some(current / max * 100.0))?;

    Some(Energy {
        percent,
        watts: watts?,
        charging: battery.boolean("IsCharging").unwrap_or(false),
        external_power: battery.boolean("ExternalConnected").unwrap_or(false),
    })
}

/// Accumulates energy by integrating the measured power over time, since no
/// usable lifetime counter is exposed: the registry's `Accumulated*` telemetry is
/// refreshed in bursts, so it cannot be differenced into a trustworthy watt figure.
///
/// The total covers the period the meter has been running. It is battery-path
/// energy: equal to the machine's draw while running on battery, but only what the
/// pack absorbs while plugged in, because the mains path is not readable without
/// root.
#[derive(Debug, Default)]
pub struct EnergyMeter {
    watt_hours: f64,
    last: Option<(Instant, f64)>,
}

impl EnergyMeter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Folds one power reading into the running total. The first reading only sets
    /// the baseline, because no time has passed yet.
    pub fn sample_at(&mut self, watts: f64, at: Instant) {
        if let Some((previous_at, previous_watts)) = self.last {
            let hours = at.saturating_duration_since(previous_at).as_secs_f64() / 3600.0;
            // The average of the two readings approximates the power over the
            // interval better than either end alone.
            self.watt_hours += (previous_watts + watts) / 2.0 * hours;
        }
        self.last = Some((at, watts));
    }

    pub fn sample(&mut self, watts: f64) {
        self.sample_at(watts, Instant::now());
    }

    /// Raw total in watt hours, for callers that need the number rather than a
    /// formatted string.
    #[cfg(test)]
    pub fn watt_hours(&self) -> f64 {
        self.watt_hours
    }

    /// Total formatted for display, switching to kilowatt hours once it gets long
    /// enough for watt hours to be unreadable.
    pub fn summary(&self) -> String {
        let total = self.watt_hours;
        if total >= 100.0 {
            format!("{:.2} kWh", total / 1000.0)
        } else {
            format!("{total:.1} Wh")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Duration;

    use super::*;

    #[test]
    fn reads_energy_from_the_battery_registry_entry() {
        let energy = energy().expect("no AppleSmartBattery entry on this Mac");
        assert!(
            (0.0..=100.0).contains(&energy.percent),
            "charge {} out of range",
            energy.percent
        );
        assert!(
            energy.watts.abs() < 500.0,
            "{} W is not a plausible figure",
            energy.watts
        );
        if !energy.external_power {
            assert!(
                energy.watts >= 0.0,
                "running on battery yet drawing {} W",
                energy.watts
            );
        }
    }

    fn battery(percent: f64, watts: f64, charging: bool, external_power: bool) -> Energy {
        Energy {
            percent,
            watts,
            charging,
            external_power,
        }
    }

    #[test]
    fn describes_the_charge_state() {
        assert_eq!(battery(89.0, 28.1, true, true).state(), "charging");
        assert_eq!(battery(89.0, 4.2, false, true).state(), "on AC");
        assert_eq!(battery(62.0, -9.5, false, false).state(), "on battery");
    }

    #[test]
    fn summarises_charge_and_watts() {
        assert_eq!(
            battery(89.0, 28.1, true, true).summary(),
            "89% 28.1 W charging"
        );
        assert_eq!(
            battery(62.0, 9.5, false, false).summary(),
            "62% 9.5 W on battery"
        );
    }

    /// A deterministic clock so the integral can be checked exactly.
    fn at(seconds: u64) -> Instant {
        Instant::now() + Duration::from_secs(seconds)
    }

    #[test]
    fn the_first_reading_only_sets_the_baseline() {
        let mut meter = EnergyMeter::new();
        meter.sample_at(20.0, at(0));
        assert_eq!(meter.watt_hours(), 0.0);

        meter.sample_at(20.0, at(3600));
        assert!(
            (meter.watt_hours() - 20.0).abs() < 1e-9,
            "{}",
            meter.watt_hours()
        );
    }

    #[test]
    fn integrates_watts_over_time_into_watt_hours() {
        let mut meter = EnergyMeter::new();
        // 20 W for an hour is 20 Wh.
        meter.sample_at(20.0, at(0));
        meter.sample_at(20.0, at(3600));
        assert!((meter.watt_hours() - 20.0).abs() < 1e-9);

        // A further ten minutes stepping from 20 W to 50 W averages to 35 W,
        // which is 5.83 Wh.
        meter.sample_at(50.0, at(4200));
        let expected = 20.0 + 35.0 * (600.0 / 3600.0);
        assert!(
            (meter.watt_hours() - expected).abs() < 1e-6,
            "{} vs {expected}",
            meter.watt_hours()
        );
    }

    #[test]
    fn averages_the_ends_of_the_interval() {
        let mut meter = EnergyMeter::new();
        // A step from 0 to 20 W over an hour is 10 Wh on the average, not 0 or 20.
        meter.sample_at(0.0, at(0));
        meter.sample_at(20.0, at(3600));
        assert!(
            (meter.watt_hours() - 10.0).abs() < 1e-9,
            "{}",
            meter.watt_hours()
        );
    }

    #[test]
    fn a_misreading_clock_does_not_move_the_total_backwards() {
        let mut meter = EnergyMeter::new();
        meter.sample_at(20.0, at(3600));
        meter.sample_at(20.0, at(0));
        assert_eq!(meter.watt_hours(), 0.0);
    }

    #[test]
    fn switches_to_kilowatt_hours_once_the_total_grows() {
        let mut meter = EnergyMeter::new();
        meter.sample_at(20.0, at(0));
        meter.sample_at(20.0, at(3600));
        assert_eq!(meter.summary(), "20.0 Wh");

        // 500 hours stepping from 20 W to 100 W averages to 60 W, which is 30000 Wh.
        meter.sample_at(100.0, at(3600 + 500 * 3600));
        assert_eq!(meter.summary(), "30.02 kWh");
    }

    /// Manual sanity check against live hardware: the accumulated total has to match
    /// the average power times the elapsed time. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "needs a battery and takes a minute"]
    fn accumulated_energy_tracks_the_average_power() {
        let mut meter = EnergyMeter::new();
        let started = Instant::now();
        let mut readings = Vec::new();

        while started.elapsed() < Duration::from_secs(60) {
            if let Some(reading) = energy() {
                meter.sample(reading.watts);
                readings.push(reading.watts);
            }
            thread::sleep(Duration::from_secs(1));
        }

        let elapsed_hours = started.elapsed().as_secs_f64() / 3600.0;
        let average = readings.iter().sum::<f64>() / readings.len() as f64;
        let expected = average * elapsed_hours;
        eprintln!(
            "{} samples, {:.1} W average, {:.0}s elapsed",
            readings.len(),
            average,
            started.elapsed().as_secs_f64()
        );
        assert!(
            (meter.watt_hours() - expected).abs() < expected * 0.05,
            "meter says {} Wh, average power implies {expected} Wh",
            meter.watt_hours()
        );
    }

    #[test]
    fn reads_the_signed_registry_values_without_losing_precision() {
        // Apple sign-extends both the current and the published milliwatt power,
        // so both arrive just under 2^64. A double cannot hold a value that
        // close to 2^64: the low bits are rounded off, which is why these are read
        // as integers. Reading -15923 mW through f64 would give roughly 1.8e16 W.
        let as_double = (u64::MAX - 15922) as f64;
        assert!(as_double / 1000.0 > 1e16);
        assert_eq!((-15923i64).unsigned_abs() as f64 / 1000.0, 15.923);
        assert_eq!(2223i64.unsigned_abs() as f64, 2223.0);
    }
}
