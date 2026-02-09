//! Helper for spawning threads with FreeRTOS task names
//!
//! Rust's `std::thread::Builder::name()` sets the pthread name after creation,
//! but ESP-IDF creates the FreeRTOS task at pthread creation time with the
//! default name. This module uses `ThreadSpawnConfiguration` to set the name
//! before spawning.

use esp_idf_hal::cpu::Core;
use esp_idf_hal::task::thread::ThreadSpawnConfiguration;
use std::ffi::CStr;
use std::thread::JoinHandle;

/// Spawn a thread with a FreeRTOS task name.
///
/// FreeRTOS task names are limited to 16 characters including the null terminator.
///
/// # Example
/// ```ignore
/// spawn_named(c"my_task", || { /* ... */ });
/// ```
pub fn spawn_named<F, T>(name: &'static CStr, f: F) -> JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    // Get current config to restore after spawn
    let prev_conf = ThreadSpawnConfiguration::get();

    // Create new config with our name
    let conf = ThreadSpawnConfiguration {
        name: Some(name.to_bytes_with_nul()),
        ..Default::default()
    };
    conf.set()
        .expect("Failed to set thread spawn configuration");

    // Spawn the thread
    let handle = std::thread::spawn(f);

    // Restore previous config (or default if none was set)
    if let Some(prev) = prev_conf {
        prev.set()
            .expect("Failed to restore thread spawn configuration");
    }

    handle
}

/// Spawn a thread with a FreeRTOS task name, pinned to a specific core.
///
/// This is useful for separating time-critical tasks (like LED control) from
/// WiFi processing, which typically runs on Core 0.
///
/// FreeRTOS task names are limited to 16 characters including the null terminator.
///
/// # Example
/// ```ignore
/// spawn_named_on_core(c"led_task", Core::Core1, || { /* ... */ });
/// ```
pub fn spawn_named_on_core<F, T>(name: &'static CStr, core: Core, f: F) -> JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    // Get current config to restore after spawn
    let prev_conf = ThreadSpawnConfiguration::get();

    // Create new config with our name and core pinning
    let conf = ThreadSpawnConfiguration {
        name: Some(name.to_bytes_with_nul()),
        pin_to_core: Some(core),
        ..Default::default()
    };
    conf.set()
        .expect("Failed to set thread spawn configuration");

    // Spawn the thread
    let handle = std::thread::spawn(f);

    // Restore previous config (or default if none was set)
    if let Some(prev) = prev_conf {
        prev.set()
            .expect("Failed to restore thread spawn configuration");
    }

    handle
}
