//! OTA (Over-The-Air) firmware update support.
//!
//! The browser fetches firmware binaries from GitHub Releases and uploads them
//! to the device via `POST /api/ota/upload`. This module handles writing the
//! received data to the inactive OTA partition and rebooting into the new image.

use anyhow::Result;
use esp_idf_svc::ota::EspOta;
use log::info;
use std::sync::atomic::{AtomicU8, Ordering};

/// OTA status: idle (no operation in progress)
pub const OTA_STATUS_IDLE: u8 = 0;
/// OTA status: downloading firmware from remote URL
pub const OTA_STATUS_DOWNLOADING: u8 = 1;
/// OTA status: writing firmware to flash
pub const OTA_STATUS_FLASHING: u8 = 2;
/// OTA status: complete, about to reboot
pub const OTA_STATUS_DONE: u8 = 3;
/// OTA status: error occurred (see `ota_error` for details)
pub const OTA_STATUS_ERROR: u8 = 255;

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

/// Download firmware from a URL and write it to the OTA partition.
///
/// Uses the ESP-IDF HTTP client with TLS certificate bundle to fetch the
/// binary, then streams it directly into `perform_ota`. Progress is reported
/// via the provided atomic status/progress fields.
pub fn download_and_update(
    url: &str,
    ota_status: &AtomicU8,
    ota_progress: &AtomicU8,
) -> Result<()> {
    use embedded_svc::io::Read;
    use esp_idf_svc::http::client::{
        Configuration as HttpConfig, EspHttpConnection, FollowRedirectsPolicy,
    };

    ota_status.store(OTA_STATUS_DOWNLOADING, Ordering::Relaxed);
    ota_progress.store(0, Ordering::Relaxed);

    info!("OTA: downloading from {url}");

    let mut conn = EspHttpConnection::new(&HttpConfig {
        crt_bundle_attach: Some(esp_idf_svc::sys::esp_crt_bundle_attach),
        timeout: Some(core::time::Duration::from_secs(60)),
        follow_redirects_policy: FollowRedirectsPolicy::FollowAll,
        buffer_size: Some(4096),
        ..Default::default()
    })?;

    conn.initiate_request(embedded_svc::http::Method::Get, url, &[])?;

    conn.initiate_response()?;

    let status = conn.status();
    if status != 200 {
        anyhow::bail!("HTTP {status} from firmware URL");
    }

    let content_length: usize = conn
        .header("Content-Length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    if content_length == 0 {
        anyhow::bail!("No Content-Length in firmware response");
    }

    info!("OTA: firmware size: {content_length} bytes");
    ota_status.store(OTA_STATUS_FLASHING, Ordering::Relaxed);

    let mut downloaded: usize = 0;

    perform_ota(
        |buf| {
            let n =
                Read::read(&mut conn, buf).map_err(|e| anyhow::anyhow!("download read: {e}"))?;
            downloaded += n;
            #[allow(clippy::cast_possible_truncation)]
            let pct = (downloaded * 100 / content_length).min(100) as u8;
            ota_progress.store(pct, Ordering::Relaxed);
            Ok(n)
        },
        content_length,
    )?;

    ota_status.store(OTA_STATUS_DONE, Ordering::Relaxed);
    ota_progress.store(100, Ordering::Relaxed);
    info!("OTA: download and flash complete");
    Ok(())
}
