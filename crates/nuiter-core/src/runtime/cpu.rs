//! The PC's cores as Windows describes them. On hybrid CPUs (Intel's P- and E-cores) a core reports an efficiency
//! class; the cores of the highest class are the performance cores. llama.cpp splits a matrix product evenly across its
//! threads, so a thread on an efficiency core finishes last and the others wait: the CPU path runs one thread per
//! performance core, without their second hyper-threads.

use std::sync::OnceLock;

use windows::Win32::System::SystemInformation::{GetLogicalProcessorInformationEx, RelationProcessorCore};

/// Physical cores of the highest efficiency class (every core, on a CPU that isn't hybrid); the logical processor count
/// halved when Windows can't say.
pub fn performance_cores() -> usize {
    static CORES: OnceLock<usize> = OnceLock::new();
    *CORES.get_or_init(count)
}

fn fallback() -> usize {
    (std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2) / 2).max(1)
}

fn count() -> usize {
    let mut length = 0u32;
    // SAFETY: the first call only asks for the size; the second fills a buffer of that size.
    unsafe {
        let _ = GetLogicalProcessorInformationEx(RelationProcessorCore, None, &mut length);
        if length == 0 {
            return fallback();
        }
        let mut buffer = vec![0u8; length as usize];
        if GetLogicalProcessorInformationEx(RelationProcessorCore, Some(buffer.as_mut_ptr() as *mut _), &mut length).is_err() {
            return fallback();
        }
        // SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX: Relationship (4 bytes), Size (4), then PROCESSOR_RELATIONSHIP:
        // Flags (1), EfficiencyClass (1), …
        let mut classes = vec![];
        let mut offset = 0usize;
        while offset + 10 <= length as usize {
            let size = u32::from_le_bytes(buffer[offset + 4..offset + 8].try_into().unwrap()) as usize;
            if size == 0 {
                break;
            }
            let relation = u32::from_le_bytes(buffer[offset..offset + 4].try_into().unwrap());
            if relation == RelationProcessorCore.0 as u32 {
                classes.push(buffer[offset + 9]);
            }
            offset += size;
        }
        match classes.iter().max() {
            Some(top) => classes.iter().filter(|c| *c == top).count(),
            None => fallback(),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn performance_cores_are_counted() {
        let cores = super::performance_cores();
        assert!(cores >= 1 && cores <= std::thread::available_parallelism().unwrap().get());
    }
}
