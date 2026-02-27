//! CPU usage monitoring using FreeRTOS runtime metrics
//!
//! Tracks per-task CPU time deltas and prints usage percentages.

use crate::thread_util;
use esp_idf_sys::{
    configNUM_CORES, esp_timer_get_time, uxTaskGetNumberOfTasks, uxTaskGetSystemState,
    TaskStatus_t, UBaseType_t,
};
use log::info;
use std::collections::HashMap;

/// Previous snapshot of task runtime counters, keyed by task handle (as usize)
type TaskSnapshots = HashMap<usize, u64>;

/// Format stack free as "X/Y" if we know the total, otherwise just "X".
fn format_stack(free: usize, total: Option<usize>) -> String {
    match total {
        Some(t) => format!("{free}/{t}"),
        None => format!("{free}"),
    }
}

/// Get the current time in microseconds
fn get_time_us() -> u64 {
    // SAFETY: esp_timer_get_time is safe to call
    // Timer returns microseconds since boot - always non-negative
    #[allow(clippy::cast_sign_loss)]
    unsafe {
        esp_timer_get_time() as u64
    }
}

/// Get the number of CPU cores
fn get_num_cores() -> u32 {
    configNUM_CORES
}

/// Collect current task states and print CPU usage deltas
pub fn print_cpu_usage_deltas(prev_snapshots: &mut TaskSnapshots, prev_total: &mut u64) {
    // Get number of tasks
    let num_tasks = unsafe { uxTaskGetNumberOfTasks() } as usize;
    if num_tasks == 0 {
        return;
    }

    // Allocate buffer for task states (with some headroom for tasks created during call)
    let mut task_array: Vec<TaskStatus_t> = vec![unsafe { std::mem::zeroed() }; num_tasks + 5];
    let mut total_runtime: u32 = 0;

    // Get system state
    // SAFETY: We provide a properly sized buffer and valid pointer for total_runtime
    let tasks_returned = unsafe {
        uxTaskGetSystemState(
            task_array.as_mut_ptr(),
            UBaseType_t::try_from(task_array.len()).expect("task count fits in u32"),
            &raw mut total_runtime,
        )
    } as usize;

    if tasks_returned == 0 {
        return;
    }

    // Get current wall-clock time in microseconds
    let current_total = get_time_us();
    let delta_total_us = current_total.saturating_sub(*prev_total);

    if delta_total_us == 0 {
        *prev_total = current_total;
        return;
    }

    // Calculate number of cores for percentage calculation
    let _num_cores = get_num_cores();

    // Build current snapshot and calculate deltas
    let mut current_snapshots: TaskSnapshots = HashMap::with_capacity(tasks_returned);
    let mut usages: Vec<(String, usize, f32, usize, Option<usize>)> =
        Vec::with_capacity(tasks_returned);

    for task in task_array.iter().take(tasks_returned) {
        let handle = task.xHandle as usize;
        let runtime = u64::from(task.ulRunTimeCounter);

        // Get task name
        // SAFETY: pcTaskName is a null-terminated C string
        let name = unsafe {
            std::ffi::CStr::from_ptr(task.pcTaskName)
                .to_string_lossy()
                .into_owned()
        };

        // Calculate delta from previous snapshot
        let prev_runtime = prev_snapshots.get(&handle).copied().unwrap_or(0);
        let delta_runtime = runtime.saturating_sub(prev_runtime);

        // Calculate percentage relative to one core (100% = one full core)
        // A task using both cores fully would show 200% on a dual-core system
        // Precision loss is fine - we only display 1 decimal place
        #[allow(clippy::cast_precision_loss)]
        let percentage = if delta_total_us > 0 {
            (delta_runtime as f32 / delta_total_us as f32) * 100.0
        } else {
            0.0
        };

        // HWM is in words (4 bytes each on ESP32-S3)
        let hwm_bytes = usize::try_from(task.usStackHighWaterMark).expect("HWM fits in usize")
            * std::mem::size_of::<esp_idf_sys::StackType_t>();

        // Look up allocated stack size from our registry
        let total_stack = thread_util::get_stack_size(&name);

        current_snapshots.insert(handle, runtime);
        usages.push((
            name,
            task.xTaskNumber as usize,
            percentage,
            hwm_bytes,
            total_stack,
        ));
    }

    // Sort by CPU usage descending
    usages.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

    // Print header
    info!("CPU usage (5s delta) + stack HWM:");

    // Print each task
    for (name, task_id, percentage, hwm_bytes, total_stack) in &usages {
        if *percentage >= 0.1 {
            let stack_str = format_stack(*hwm_bytes, *total_stack);
            info!("  {name:16} (#{task_id:2}): {percentage:5.1}%  stack_free={stack_str}");
        }
    }

    // Print stack HWM for idle/low-CPU tasks that were skipped above
    let low_cpu: Vec<_> = usages
        .iter()
        .filter(|(_, _, pct, _, _)| *pct < 0.1)
        .collect();
    if !low_cpu.is_empty() {
        info!("  Stack HWM for idle tasks:");
        for (name, task_id, _, hwm_bytes, total_stack) in low_cpu {
            let stack_str = format_stack(*hwm_bytes, *total_stack);
            info!("  {name:16} (#{task_id:2}):          stack_free={stack_str}");
        }
    }

    // Update previous state
    *prev_snapshots = current_snapshots;
    *prev_total = current_total;
}
