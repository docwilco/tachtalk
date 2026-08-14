//! Helper for spawning threads with FreeRTOS task names
//!
//! Rust's `std::thread::Builder::name()` sets the pthread name after creation,
//! but ESP-IDF creates the FreeRTOS task at pthread creation time with the
//! default name. This module uses `ThreadSpawnConfiguration` to set the name
//! before spawning.
//!
//! Stack sizes can be overridden at runtime via NVS (namespace `stack_cfg`).
//! Each key is a task name (e.g. `sse_srv`, `dongle`) and the value is a
//! `u32` stack size in bytes. Call [`load_stack_overrides`] once after NVS
//! init. The override replaces the compiled default entirely.

use esp_idf_hal::cpu::Core;
use esp_idf_hal::task::thread::{MallocCap, ThreadSpawnConfiguration};
use esp_idf_svc::nvs::{EspNvs, EspNvsPartition, NvsCustom};
use esp_idf_svc::sys::{heap_caps_print_heap_info, MALLOC_CAP_INTERNAL};
use log::{info, warn};
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

// ── NVS stack size overrides ─────────────────────────────────────────────────

const STACK_NVS_PARTITION: &str = "stack_cfg";
const STACK_NVS_NAMESPACE: &str = "stack_cfg";

/// NVS-loaded stack size overrides, keyed by task name.
static STACK_OVERRIDES: Mutex<Option<HashMap<String, usize>>> = Mutex::new(None);

/// Task names that we look up in NVS. Must match the `c"..."` names passed to
/// `spawn_named` / `spawn_named_on_core` (without the null terminator).
const TASK_NAMES: &[&str] = &[
    "sse_srv",
    "dongle",
    "cache_mgr",
    "led_disp",
    "obd2_proxy",
    "wifi_mgr",
    "status_led",
    "controls",
    "http_srv",
];

/// Load stack size overrides from the `stack_cfg` NVS partition.
///
/// Each key is a task name (max 15 chars, matching FreeRTOS limit) and the
/// value is a `u32` stack size in bytes. Missing keys are silently skipped.
/// Call once early in main, before spawning tasks.
pub fn load_stack_overrides() {
    let partition = match EspNvsPartition::<NvsCustom>::take(STACK_NVS_PARTITION) {
        Ok(p) => p,
        Err(e) => {
            warn!("Failed to open NVS partition '{STACK_NVS_PARTITION}': {e}");
            return;
        }
    };

    let nvs = match EspNvs::new(partition, STACK_NVS_NAMESPACE, false) {
        Ok(n) => n,
        Err(e) => {
            warn!("Failed to open NVS namespace '{STACK_NVS_NAMESPACE}': {e}");
            return;
        }
    };

    let mut overrides = HashMap::new();
    for &name in TASK_NAMES {
        match nvs.get_u32(name) {
            Ok(Some(val)) => {
                let size = val as usize;
                info!("Stack override: {name} = {size}");
                overrides.insert(name.to_owned(), size);
            }
            Ok(None) => {} // No override, use compiled default
            Err(e) => warn!("Failed to read stack override for '{name}': {e}"),
        }
    }

    if overrides.is_empty() {
        info!("No stack size overrides in NVS");
    } else {
        info!("Loaded {} stack size override(s) from NVS", overrides.len());
    }

    *STACK_OVERRIDES.lock().unwrap() = Some(overrides);
}

/// Return the effective stack size for a task: NVS override if set, otherwise
/// the compiled default.
pub fn stack_size(name: &CStr, default: usize) -> usize {
    let name_str = name.to_str().unwrap_or("");
    STACK_OVERRIDES
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().and_then(|map| map.get(name_str).copied()))
        .unwrap_or(default)
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
