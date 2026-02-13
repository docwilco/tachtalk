//! OTA (Over-The-Air) firmware update support.
//!
//! The browser fetches firmware binaries from GitHub Releases and uploads them
//! to the device via `POST /api/ota/upload`. This module handles writing the
//! received data to the inactive OTA partition and rebooting into the new image.

use anyhow::Result;
use esp_idf_svc::ota::EspOta;
use log::info;

/// Firmware version from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Mark the currently running OTA slot as valid.
///
/// Must be called early in `main()` when rollback is enabled
/// (`CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE=y`). If the new firmware crashes
/// before this call, the bootloader reverts to the previous image.
pub fn mark_running_slot_valid() -> Result<()> {
    let mut ota = EspOta::new()?;
    ota.mark_running_slot_valid()?;
    info!("OTA: running slot marked valid");
    Ok(())
}

/// Information about the currently running firmware.
#[derive(serde::Serialize)]
pub struct FirmwareInfo {
    pub version: &'static str,
    pub variant: &'static str,
}

/// Return metadata about the running firmware.
pub fn firmware_info() -> FirmwareInfo {
    FirmwareInfo {
        version: VERSION,
        variant: crate::FIRMWARE_VARIANT,
    }
}

/// Write firmware data from an HTTP request body to the OTA update partition.
///
/// `reader` is called repeatedly with a mutable buffer; it must fill the buffer
/// and return the number of bytes read (0 signals EOF). `total_size` is the
/// `Content-Length` of the upload, used to pre-erase only the required flash
/// space.
///
/// On success the new image is activated and the caller should reboot.
pub fn perform_ota<F>(mut reader: F, total_size: usize) -> Result<()>
where
    F: FnMut(&mut [u8]) -> Result<usize>,
{
    info!("OTA: starting update, size={total_size}");

    let mut ota = EspOta::new()?;
    let mut update = ota.initiate_update()?;

    let mut buf = [0u8; 4096];
    let mut written: usize = 0;
    let mut last_pct: u8 = 0;

    loop {
        let n = reader(&mut buf)?;
        if n == 0 {
            break;
        }
        update.write(&buf[..n])?;
        written += n;

        // Log progress at each 10% increment
        #[allow(clippy::cast_possible_truncation)]
        let pct = if total_size > 0 {
            (written * 100 / total_size) as u8
        } else {
            0
        };
        if pct / 10 > last_pct / 10 {
            info!("OTA: {written}/{total_size} bytes ({pct}%)");
            last_pct = pct;
        }
    }

    if written == 0 {
        anyhow::bail!("OTA: received 0 bytes");
    }

    info!("OTA: finalizing update ({written} bytes written)");
    update.complete()?;
    info!("OTA: update complete, reboot required");
    Ok(())
}
