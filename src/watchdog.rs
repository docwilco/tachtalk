//! Task watchdog integration for monitoring thread health.
//!
//! Provides a simple wrapper around ESP-IDF's Task Watchdog Timer (TWDT)
//! for registering threads and feeding the watchdog.

use esp_idf_svc::sys::{
    esp_task_wdt_add_user, esp_task_wdt_delete_user, esp_task_wdt_reset_user,
    esp_task_wdt_user_handle_t,
};
use log::{debug, error};
use std::ffi::CString;

/// A handle to a registered watchdog user. Automatically unregisters on drop.
pub struct WatchdogHandle {
    handle: esp_task_wdt_user_handle_t,
    // Keep the CString alive - ESP-IDF keeps a pointer to the name string
    #[allow(dead_code)]
    c_name: CString,
}

impl WatchdogHandle {
    /// Register a new watchdog user with the given name.
    /// 
    /// The name should be descriptive (e.g., "web_server", "obd2_proxy").
    /// Returns None if registration fails.
    pub fn register(name: &str) -> Option<Self> {
        let c_name = CString::new(name).ok()?;
        let mut handle: esp_task_wdt_user_handle_t = std::ptr::null_mut();
        
        let result = unsafe { esp_task_wdt_add_user(c_name.as_ptr(), &mut handle) };
        
        if result == 0 {
            debug!("Watchdog: registered user '{name}'");
            Some(Self {
                handle,
                c_name,
            })
        } else {
            error!("Watchdog: failed to register user '{name}': error code {result}");
            None
        }
    }
    
    /// Feed the watchdog to prevent timeout.
    /// 
    /// This must be called periodically (within the watchdog timeout period)
    /// to indicate the thread is still running properly.
    pub fn feed(&self) {
        let result = unsafe { esp_task_wdt_reset_user(self.handle) };
        if result != 0 {
            error!("Watchdog: failed to feed '{:?}'", self.c_name);
        }
    }
}

impl Drop for WatchdogHandle {
    fn drop(&mut self) {
        debug!("Watchdog: unregistering user '{:?}'", self.c_name);
        let result = unsafe { esp_task_wdt_delete_user(self.handle) };
        if result != 0 {
            error!("Watchdog: failed to unregister '{:?}': error code {result}", self.c_name);
        }
    }
}
