use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString};
use objc2_io_kit::{
    io_object_t, IOIteratorNext, IOMainPort, IOObjectRelease, IORegistryEntryCreateCFProperty,
    IOServiceGetMatchingServices, IOServiceMatching,
};

/// Battery charge and state, read from the `AppleSmartBattery` registry entry.
///
/// macOS has no supported power API: `pmset` reports state transitions but no
/// numbers, so the figures come straight from IOKit. That needs neither root nor
/// a helper process.
#[derive(Clone, Copy, Debug)]
pub struct Energy {
    /// Charge level, 0..=100.
    pub percent: f64,
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

    /// Charge level and state, e.g. `89% charging`.
    pub fn summary(&self) -> String {
        format!("{:.0}% {}", self.percent, self.state())
    }
}

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

    fn boolean(&self, key: &str) -> Option<bool> {
        unsafe {
            let key: CFRetained<CFString> = CFString::from_str(key);
            let value = IORegistryEntryCreateCFProperty(self.entry, Some(&key), None, 0)?;
            value.downcast_ref::<CFBoolean>().map(CFBoolean::as_bool)
        }
    }
}

impl Drop for Battery {
    fn drop(&mut self) {
        IOObjectRelease(self.entry);
    }
}

pub fn energy() -> Option<Energy> {
    let battery = Battery::find()?;

    // CurrentCapacity counts against MaxCapacity, which is 100 on most packs but
    // not on every one, so the share is derived rather than read as a percentage.
    let percent = battery
        .number("CurrentCapacity")
        .zip(battery.number("MaxCapacity"))
        .and_then(|(current, max)| (max > 0.0).then_some(current / max * 100.0))?;

    Some(Energy {
        percent,
        charging: battery.boolean("IsCharging").unwrap_or(false),
        external_power: battery.boolean("ExternalConnected").unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_charge_from_the_battery_registry_entry() {
        let energy = energy().expect("no AppleSmartBattery entry on this Mac");
        assert!(
            (0.0..=100.0).contains(&energy.percent),
            "charge {} out of range",
            energy.percent
        );
    }

    fn battery(percent: f64, charging: bool, external_power: bool) -> Energy {
        Energy {
            percent,
            charging,
            external_power,
        }
    }

    #[test]
    fn describes_the_charge_state() {
        assert_eq!(battery(89.0, true, true).state(), "charging");
        assert_eq!(battery(89.0, false, true).state(), "on AC");
        assert_eq!(battery(62.0, false, false).state(), "on battery");
    }

    #[test]
    fn summarises_charge_and_state() {
        assert_eq!(battery(89.0, true, true).summary(), "89% charging");
        assert_eq!(battery(62.0, false, false).summary(), "62% on battery");
    }
}
