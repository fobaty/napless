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
    /// Battery power in watts, signed by the direction of the current as the pack
    /// reports it. The sign does not reliably mean charging or discharging, so the
    /// charge state is taken from the boolean flag instead.
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

    /// Charge level and live watt draw, e.g. `89% +28.1 W charging`.
    pub fn summary(&self) -> String {
        format!("{:.0}% {:+.1} W {}", self.percent, self.watts, self.state())
    }
}

/// Amperage is in milliamps and voltage in millivolts, so their product needs
/// two factors of a thousand to arrive at watts.
const MILLIWATTS_PER_WATT: f64 = 1000.0 * 1000.0;

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
}

impl Drop for Battery {
    fn drop(&mut self) {
        IOObjectRelease(self.entry);
    }
}

pub fn energy() -> Option<Energy> {
    let battery = Battery::find()?;

    // Both are needed for a watt figure: one without the other has no unit.
    let watts = battery
        .amperage()
        .zip(battery.number("AppleRawBatteryVoltage"))
        .map(|(milliamps, millivolts)| milliamps * millivolts / MILLIWATTS_PER_WATT);

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

#[cfg(test)]
mod tests {
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
            "89% +28.1 W charging"
        );
        assert_eq!(
            battery(62.0, -9.5, false, false).summary(),
            "62% -9.5 W on battery"
        );
    }

    #[test]
    fn reads_amperage_without_losing_the_sign() {
        // A double cannot hold a value within 1557 of 2^64: the low bits are
        // rounded off, which is why the registry value is read as an integer.
        let as_double = (u64::MAX - 1556) as f64;
        assert_ne!(as_double as u64 as i64, -1557);
        assert_eq!((-1557i64) as f64 as i64, -1557);
        assert_eq!(2223.0_f64 as i64, 2223);
    }
}
