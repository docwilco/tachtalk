//! Heap diagnostics for debugging internal SRAM usage.
//!
//! Provides a function to log internal and PSRAM heap stats periodically,
//! and `heap_caps_print_heap_info` is used in `thread_util` for OOM dumps.

use esp_idf_svc::sys::{
    heap_caps_get_free_size, heap_caps_get_minimum_free_size, MALLOC_CAP_INTERNAL,
    MALLOC_CAP_SPIRAM,
};
use log::info;

/// Log current heap stats for internal SRAM and PSRAM.
pub fn log_heap_stats() {
    let internal_free = unsafe { heap_caps_get_free_size(MALLOC_CAP_INTERNAL) };
    let internal_min = unsafe { heap_caps_get_minimum_free_size(MALLOC_CAP_INTERNAL) };
    let spiram_free = unsafe { heap_caps_get_free_size(MALLOC_CAP_SPIRAM) };
    let spiram_min = unsafe { heap_caps_get_minimum_free_size(MALLOC_CAP_SPIRAM) };

    info!(
        "Heap: internal free={} KB (min={} KB) | PSRAM free={} KB (min={} KB)",
        internal_free / 1024,
        internal_min / 1024,
        spiram_free / 1024,
        spiram_min / 1024,
    );
}
