//! Task watchdog integration for monitoring thread health.
//!
//! Provides a simple wrapper around ESP-IDF's Task Watchdog Timer (TWDT)
//! for registering threads and feeding the watchdog.

use esp_idf_svc::sys::{
    esp_task_wdt_add_user, esp_task_wdt_delete_user, esp_task_wdt_reset_user,
    esp_task_wdt_user_handle_t,
};
use log::{debug, error};
use std::ffi::CStr;

/// A handle to a registered watchdog user. Automatically unregisters on drop.
pub struct WatchdogHandle {
    handle: esp_task_wdt_user_handle_t,
    name: &'static CStr,
}

impl WatchdogHandle {
    /// Register a new watchdog user with the given name.
    ///
    /// The name should be descriptive (e.g., `c"web_server"`, `c"obd2_proxy"`).
    ///
    /// # Panics
    /// Panics if registration fails (critical system error).
    pub fn register(name: &'static CStr) -> Self {
        let mut handle: esp_task_wdt_user_handle_t = std::ptr::null_mut();

        let result = unsafe { esp_task_wdt_add_user(name.as_ptr(), &mut handle) };

        if result == 0 {
            debug!("Watchdog: registered user '{name:?}'");
            Self { handle, name }
        } else {
            panic!("Watchdog: failed to register user '{name:?}': error code {result}");
        }
    }

    /// Feed the watchdog to prevent timeout.
    ///
    /// This must be called periodically (within the watchdog timeout period)
    /// to indicate the thread is still running properly.
    pub fn feed(&self) {
        let result = unsafe { esp_task_wdt_reset_user(self.handle) };
        if result != 0 {
            error!("Watchdog: failed to feed '{:?}'", self.name);
        }
    }
}

impl Drop for WatchdogHandle {
    fn drop(&mut self) {
        debug!("Watchdog: unregistering user '{:?}'", self.name);
        let result = unsafe { esp_task_wdt_delete_user(self.handle) };
        if result != 0 {
            error!(
                "Watchdog: failed to unregister '{:?}': error code {result}",
                self.name
            );
        }
    }
}
