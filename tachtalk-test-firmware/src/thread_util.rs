//! Helper for spawning threads with FreeRTOS task names
//!
//! Rust's `std::thread::Builder::name()` sets the pthread name after creation,
//! but ESP-IDF creates the FreeRTOS task at pthread creation time with the
//! default name. This module uses `ThreadSpawnConfiguration` to set the name
//! before spawning.

use esp_idf_hal::task::thread::{MallocCap, ThreadSpawnConfiguration};
use std::ffi::CStr;
use std::thread::JoinHandle;

/// Where to allocate the thread's stack memory.
#[derive(Debug, Clone, Copy)]
pub enum StackMemory {
    /// Use internal SRAM (faster, but limited to ~300 KB shared with WiFi/DMA).
    Internal,
    /// Use PSRAM/SPIRAM (slower, but 8 MB available on N16R8).
    Spiram,
}

impl StackMemory {
    fn to_caps(self) -> enumset::EnumSet<MallocCap> {
        match self {
            Self::Internal => enumset::enum_set!(MallocCap::Internal | MallocCap::Cap8bit),
            Self::Spiram => enumset::enum_set!(MallocCap::Spiram | MallocCap::Cap8bit),
        }
    }
}

/// Spawn a thread with a FreeRTOS task name and explicit stack size.
///
/// FreeRTOS task names are limited to 16 characters including the null terminator.
/// `stack_size` is in bytes; use a value appropriate for the thread's workload
/// (e.g. 8192 for typical Rust tasks on ESP32).
///
/// # Example
/// ```ignore
/// spawn_named(c"my_task", 8192, StackMemory::Spiram, || { /* ... */ });
/// ```
pub fn spawn_named<F, T>(
    name: &'static CStr,
    stack_size: usize,
    memory: StackMemory,
    f: F,
) -> JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    // Get current config to restore after spawn
    let prev_conf = ThreadSpawnConfiguration::get();

    // Create new config with our name and memory caps.
    // NOTE: stack_size is intentionally 0 here. ESP-IDF's pthread_create
    // uses attr->stacksize (set by Builder::stack_size below) which overrides
    // ThreadSpawnConfiguration.stack_size. We still need ThreadSpawnConfiguration
    // for name, stack_alloc_caps, pin_to_core, and priority.
    let conf = ThreadSpawnConfiguration {
        name: Some(name.to_bytes_with_nul()),
        stack_alloc_caps: memory.to_caps(),
        ..Default::default()
    };
    conf.set()
        .expect("Failed to set thread spawn configuration");

    // Pass stack_size through Builder so it reaches pthread_attr_setstacksize.
    let result = std::thread::Builder::new().stack_size(stack_size).spawn(f);

    // Restore previous config (or default if none was set)
    if let Some(prev) = prev_conf {
        prev.set()
            .expect("Failed to restore thread spawn configuration");
    }

    result.unwrap_or_else(|e| {
        panic!("Failed to spawn thread {name:?} (stack={stack_size}, memory={memory:?}): {e}");
    })
}
