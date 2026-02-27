//! Helper for spawning threads with FreeRTOS task names
//!
//! Rust's `std::thread::Builder::name()` sets the pthread name after creation,
//! but ESP-IDF creates the FreeRTOS task at pthread creation time with the
//! default name. This module uses `ThreadSpawnConfiguration` to set the name
//! before spawning.

use esp_idf_hal::cpu::Core;
use esp_idf_hal::task::thread::{MallocCap, ThreadSpawnConfiguration};
use esp_idf_svc::sys::{heap_caps_print_heap_info, MALLOC_CAP_INTERNAL};
use std::collections::HashMap;
use std::ffi::CStr;
use std::sync::Mutex;
use std::thread::JoinHandle;

/// Global registry mapping task name → allocated stack size (bytes).
/// Populated by `spawn_named` / `spawn_named_on_core`, read by `cpu_metrics`.
static STACK_REGISTRY: Mutex<Option<HashMap<String, usize>>> = Mutex::new(None);

/// Look up the allocated stack size for a task by name.
pub fn get_stack_size(name: &str) -> Option<usize> {
    STACK_REGISTRY
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().and_then(|map| map.get(name).copied()))
}

/// Record a task's stack size in the registry.
pub fn register_stack_size(name: &CStr, stack_size: usize) {
    if let Ok(mut guard) = STACK_REGISTRY.lock() {
        let map = guard.get_or_insert_with(HashMap::new);
        map.insert(name.to_string_lossy().into_owned(), stack_size);
    }
}

/// Where to allocate the thread's stack memory.
#[derive(Debug, Clone, Copy)]
pub enum StackMemory {
    /// Use internal SRAM (faster, but limited to ~300 KB shared with WiFi/DMA).
    Internal,
    /// Use PSRAM/SPIRAM (slower, but 8 MB available on N16R8).
    SpiRam,
}

impl StackMemory {
    fn to_caps(self) -> enumset::EnumSet<MallocCap> {
        match self {
            Self::Internal => enumset::enum_set!(MallocCap::Internal | MallocCap::Cap8bit),
            Self::SpiRam => enumset::enum_set!(MallocCap::Spiram | MallocCap::Cap8bit),
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
    spawn_thread_inner(name, None, stack_size, memory, f)
}

/// Spawn a thread with a FreeRTOS task name, pinned to a specific core,
/// with an explicit stack size.
///
/// This is useful for separating time-critical tasks (like LED control) from
/// WiFi processing, which typically runs on Core 0.
///
/// FreeRTOS task names are limited to 16 characters including the null terminator.
/// `stack_size` is in bytes; use a value appropriate for the thread's workload.
///
/// # Example
/// ```ignore
/// spawn_named_on_core(c"led_task", Core::Core1, 8192, StackMemory::Spiram, || { /* ... */ });
/// ```
pub fn spawn_named_on_core<F, T>(
    name: &'static CStr,
    core: Core,
    stack_size: usize,
    memory: StackMemory,
    f: F,
) -> JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    spawn_thread_inner(name, Some(core), stack_size, memory, f)
}

fn spawn_thread_inner<F, T>(
    name: &'static CStr,
    pin_to_core: Option<Core>,
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

    // Create new config with our name, optional core pinning, and memory caps.
    // NOTE: stack_size is intentionally 0 here. ESP-IDF's pthread_create
    // uses attr->stacksize (set by Builder::stack_size below) which overrides
    // ThreadSpawnConfiguration.stack_size. We still need ThreadSpawnConfiguration
    // for name, stack_alloc_caps, pin_to_core, and priority.
    let conf = ThreadSpawnConfiguration {
        name: Some(name.to_bytes_with_nul()),
        pin_to_core,
        stack_alloc_caps: memory.to_caps(),
        ..Default::default()
    };
    conf.set()
        .expect("Failed to set thread spawn configuration");

    // Record stack size for diagnostic output
    register_stack_size(name, stack_size);

    // Pass stack_size through Builder so it reaches pthread_attr_setstacksize.
    let result = std::thread::Builder::new().stack_size(stack_size).spawn(f);

    // Restore previous config (or default if none was set)
    if let Some(prev) = prev_conf {
        prev.set()
            .expect("Failed to restore thread spawn configuration");
    }

    let core_info = pin_to_core.map_or_else(String::new, |c| format!(" on core {c:?}"));
    result.unwrap_or_else(|e| {
        // Dump internal SRAM state before panicking
        log::error!(
            "Failed to spawn thread {name:?}{core_info} (stack={stack_size}, memory={memory:?}): {e} — dumping internal heap info:"
        );
        unsafe { heap_caps_print_heap_info(MALLOC_CAP_INTERNAL) };
        panic!(
            "Failed to spawn thread {name:?}{core_info} (stack={stack_size}, memory={memory:?}): {e}"
        );
    })
}
