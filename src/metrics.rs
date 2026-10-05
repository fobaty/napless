use std::time::{Duration, Instant};

use smc_lib::io::IOService;
use smc_lib::structs::SMCVal;
use smc_lib::value::SmcValue;

// `libc` marks `mach_host_self` as deprecated in favour of the `mach2` crate,
// which does not expose `host_statistics`, so the entry point is declared here.
unsafe extern "C" {
    fn mach_host_self() -> libc::mach_port_t;
}

const BYTES_PER_GB: f64 = 1024.0 * 1024.0 * 1024.0;
const FALLBACK_PAGE_SIZE: u64 = 4096;
const CPU_STATES: usize = 4;
const IDLE_STATE: usize = libc::CPU_STATE_IDLE as usize;
const MIN_SAMPLE_GAP: Duration = Duration::from_millis(200);

#[derive(Clone, Copy, Default, Debug)]
pub struct Memory {
    pub used_gb: f64,
    pub total_gb: f64,
    pub percent: f64,
}

#[derive(Clone, Debug)]
pub enum Temperature {
    Pending,
    Value(f64),
    Unavailable(String),
}

/// CPU usage is derived from deltas between two `host_statistics` snapshots,
/// the same way Activity Monitor computes it.
#[derive(Default)]
pub struct CpuSampler {
    previous: Option<[u64; CPU_STATES]>,
    previous_at: Option<Instant>,
}

impl CpuSampler {
    /// Returns `None` for the very first sample, and whenever the counters did
    /// not move or the previous snapshot is too recent to be meaningful.
    pub fn sample(&mut self) -> Option<f64> {
        let current = cpu_ticks()?;
        let now = Instant::now();

        let percent = match (self.previous, self.previous_at) {
            (Some(previous), Some(previous_at))
                if now.duration_since(previous_at) >= MIN_SAMPLE_GAP =>
            {
                let total: u64 = (0..CPU_STATES)
                    .map(|i| current[i].saturating_sub(previous[i]))
                    .sum();
                let idle = current[IDLE_STATE].saturating_sub(previous[IDLE_STATE]);
                (total > 0).then(|| (total - idle) as f64 / total as f64 * 100.0)
            }
            _ => None,
        };

        self.previous = Some(current);
        self.previous_at = Some(now);
        percent
    }
}

fn cpu_ticks() -> Option<[u64; CPU_STATES]> {
    unsafe {
        let mut info: libc::host_cpu_load_info_data_t = std::mem::zeroed();
        let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
        let result = libc::host_statistics(
            mach_host_self(),
            libc::HOST_CPU_LOAD_INFO,
            std::ptr::addr_of_mut!(info) as *mut libc::integer_t,
            &mut count,
        );
        if result != libc::KERN_SUCCESS {
            return None;
        }
        Some(std::array::from_fn(|i| info.cpu_ticks[i] as u64))
    }
}

pub fn memory() -> Option<Memory> {
    let total_bytes = total_memory_bytes()?;
    let page_size = page_size();
    let stats = vm_statistics()?;

    // Matches what Activity Monitor counts as "Memory Used": app memory plus
    // wired memory plus the compressed footprint.
    let used_pages = stats.active_count + stats.wire_count + stats.compressor_page_count;
    let used_bytes = used_pages as f64 * page_size;

    Some(Memory {
        used_gb: used_bytes / BYTES_PER_GB,
        total_gb: total_bytes as f64 / BYTES_PER_GB,
        percent: (used_bytes / total_bytes as f64 * 100.0).clamp(0.0, 100.0),
    })
}

fn total_memory_bytes() -> Option<u64> {
    sysctl_u64(c"hw.memsize")
}

/// Apple silicon uses 16 KiB VM pages and Intel uses 4 KiB, so the size has to
/// be queried: assuming the wrong one reports a quarter of the real memory.
fn page_size() -> f64 {
    sysctl_u64(c"hw.pagesize").unwrap_or(FALLBACK_PAGE_SIZE) as f64
}

fn sysctl_u64(name: &std::ffi::CStr) -> Option<u64> {
    let mut value: u64 = 0;
    let mut size = std::mem::size_of::<u64>();
    let result = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            std::ptr::addr_of_mut!(value) as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (result == libc::KERN_SUCCESS).then_some(value)
}

fn vm_statistics() -> Option<libc::vm_statistics64_data_t> {
    unsafe {
        let mut stats: libc::vm_statistics64_data_t = std::mem::zeroed();
        let mut count = libc::HOST_VM_INFO64_COUNT;
        let result = libc::host_statistics64(
            mach_host_self(),
            libc::HOST_VM_INFO64,
            std::ptr::addr_of_mut!(stats) as *mut libc::integer_t,
            &mut count,
        );
        (result == libc::KERN_SUCCESS).then_some(stats)
    }
}

/// macOS exposes no supported temperature API, so die temperatures are read
/// straight from the System Management Controller over IOKit: no root, no
/// helper process, no sampling delay.
///
/// The key that carries the CPU die temperature differs between Intel and
/// Apple silicon, so candidates are probed once at start-up and the first one
/// that returns a sane float is reused for the rest of the session.
pub struct TemperatureProbe {
    service: Option<IOService>,
    key: Option<[u8; 4]>,
    last_good: Option<(f64, Instant)>,
    error: Option<String>,
}

/// Probed in order: Apple silicon CPU die, then the Intel equivalents.
const CANDIDATE_KEYS: [&[u8; 4]; 5] = [b"Tp0T", b"TC0P", b"TC0D", b"TC0H", b"Tp09"];

/// Values below this are sensor placeholders rather than real readings: on some
/// models an idle die reports a floor of a couple of degrees until it is loaded.
const MIN_PLAUSIBLE_CELSIUS: f64 = 10.0;
const MAX_PLAUSIBLE_CELSIUS: f64 = 120.0;
/// How long a good reading stays valid after the SMC stops returning one.
const STALE_AFTER: Duration = Duration::from_secs(5);

fn is_plausible(celsius: f64) -> bool {
    (MIN_PLAUSIBLE_CELSIUS..=MAX_PLAUSIBLE_CELSIUS).contains(&celsius)
}

/// Every Apple platform stores these keys little-endian, so the big-endian
/// interpretation of a `flt` reading is always noise.
fn decode_celsius(value: &SMCVal) -> Option<f64> {
    match value.data_value()? {
        SmcValue::F32 { le, .. } => Some(le as f64),
        SmcValue::Ioft48_16(raw) => Some((raw >> 16) as f64 + (raw & 0xFFFF) as f64 / 65536.0),
        SmcValue::U8(value) => Some(value as f64),
        SmcValue::I16(value) => Some(value as f64),
        SmcValue::U16(value) => Some(value as f64),
        SmcValue::I32(value) => Some(value as f64),
        SmcValue::U32(value) => Some(value as f64),
        _ => None,
    }
}

impl TemperatureProbe {
    pub fn start() -> Self {
        let mut probe = Self {
            service: None,
            key: None,
            last_good: None,
            error: None,
        };

        match IOService::init() {
            Ok(service) => {
                probe.key = CANDIDATE_KEYS
                    .iter()
                    .map(|key| **key)
                    .find(|key| probe.read_celsius(&service, key).is_some());
                probe.service = Some(service);

                if probe.key.is_none() {
                    probe.error = Some("no usable CPU temperature key on this Mac".to_string());
                }
            }
            Err(e) => probe.error = Some(format!("SMC unavailable: {e}")),
        }

        probe
    }

    fn read_celsius(&self, service: &IOService, key: &[u8; 4]) -> Option<f64> {
        let value = service.read_key(key).ok()?;
        let celsius = decode_celsius(&value)?;
        is_plausible(celsius).then_some(celsius)
    }

    /// Latest reading, or the reason none is available. Holds the previous
    /// value briefly when the SMC momentarily returns nothing, so the gauge
    /// does not flicker.
    pub fn poll(&mut self) -> Temperature {
        if let (Some(service), Some(key)) = (self.service.as_ref(), self.key) {
            if let Some(celsius) = self.read_celsius(service, &key) {
                self.last_good = Some((celsius, Instant::now()));
                return Temperature::Value(celsius);
            }
        }

        if let Some((celsius, at)) = self.last_good {
            if at.elapsed() < STALE_AFTER {
                return Temperature::Value(celsius);
            }
        }

        match &self.error {
            Some(reason) => Temperature::Unavailable(reason.clone()),
            None => {
                Temperature::Unavailable("CPU temperature is not reported by the SMC".to_string())
            }
        }
    }

    pub fn sensor_name(&self) -> Option<String> {
        self.key
            .map(|key| String::from_utf8_lossy(&key).to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    #[test]
    fn rejects_implausible_sensor_placeholders() {
        assert!(!is_plausible(1.5));
        assert!(!is_plausible(-4.0));
        assert!(!is_plausible(0.0));
        assert!(!is_plausible(140.0));
        assert!(is_plausible(40.0));
        assert!(is_plausible(95.5));
    }

    #[test]
    fn reads_cpu_temperature_without_root() {
        let mut probe = TemperatureProbe::start();
        assert!(probe.error.is_none(), "{:?}", probe.error);

        match probe.poll() {
            Temperature::Value(celsius) => {
                assert!(
                    (MIN_PLAUSIBLE_CELSIUS..=MAX_PLAUSIBLE_CELSIUS).contains(&celsius),
                    "{celsius} is not a sane temperature"
                );
                assert!(probe.sensor_name().is_some());
            }
            other => panic!("expected a reading, got {other:?}"),
        }
    }

    #[test]
    fn holds_the_last_reading_briefly_when_the_smc_goes_quiet() {
        let mut probe = TemperatureProbe::start();
        let Temperature::Value(first) = probe.poll() else {
            panic!("no reading available on this Mac");
        };

        probe.key = Some(*b"ZZZZ");
        match probe.poll() {
            Temperature::Value(held) => assert_eq!(held, first),
            other => panic!("expected the held reading, got {other:?}"),
        }
    }

    #[test]
    fn cpu_usage_is_a_percentage_within_bounds() {
        let mut sampler = CpuSampler::default();
        thread::sleep(Duration::from_millis(250));
        sampler.sample();
        thread::sleep(Duration::from_millis(250));
        if let Some(percent) = sampler.sample() {
            assert!((0.0..=100.0).contains(&percent), "{percent} out of range");
        }
    }

    #[test]
    fn reports_memory_totals_for_this_machine() {
        let memory = memory().expect("memory statistics unavailable");
        assert!(memory.total_gb > 0.0);
        assert!(memory.used_gb <= memory.total_gb);
    }

    #[test]
    fn page_size_comes_from_the_kernel_not_a_constant() {
        // Queried independently of `page_size()` so that re-hardcoding the
        // constant cannot make this test agree with itself.
        let mut expected: u64 = 0;
        let mut size = std::mem::size_of::<u64>();
        let result = unsafe {
            libc::sysctlbyname(
                c"hw.pagesize".as_ptr(),
                std::ptr::addr_of_mut!(expected) as *mut libc::c_void,
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        assert_eq!(result, libc::KERN_SUCCESS);
        assert_eq!(page_size() as u64, expected);
        assert!([4096, 16384, 65536].contains(&expected));
    }

    #[test]
    fn memory_used_tracks_the_page_count_and_page_size() {
        let memory = memory().expect("memory statistics unavailable");
        let stats = vm_statistics().expect("vm statistics unavailable");
        let pages = stats.active_count + stats.wire_count + stats.compressor_page_count;
        let expected = pages as f64 * page_size() / BYTES_PER_GB;
        assert!(
            (memory.used_gb - expected).abs() < 0.01,
            "used {:.2} GB does not match {pages} pages of {} bytes ({expected:.2} GB)",
            memory.used_gb,
            page_size()
        );
    }
}
