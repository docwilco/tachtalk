//! Crate-level error type replacing `anyhow`.

/// Crate-wide error type.
#[derive(Debug, derive_more::Display, derive_more::Error, derive_more::From)]
pub enum Error {
    /// ESP-IDF system error (non-IO)
    Esp(esp_idf_svc::sys::EspError),

    /// ESP-IDF IO error
    EspIo(esp_idf_svc::io::EspIOError),

    /// Standard IO error (TCP, filesystem, etc.)
    Io(std::io::Error),

    /// JSON serialization/deserialization error
    Json(serde_json::Error),

    /// WS2812 LED driver error
    Ws2812(ws2812_esp32_rmt_driver::Ws2812Esp32RmtDriverError),

    /// NVS storage not yet initialized
    #[display("NVS not initialized")]
    NvsNotInitialized,

    /// No configuration blob stored in NVS
    #[display("no config found in NVS")]
    NvsConfigNotFound,

    /// Static IP address string could not be parsed
    #[display("invalid static IP: {_0}")]
    #[from(ignore)]
    InvalidStaticIp(#[error(not(source))] String),

    /// OTA upload contained zero bytes
    #[display("OTA: received 0 bytes")]
    OtaZeroBytes,

    /// OTA firmware download got a non-200 HTTP status
    #[display("HTTP {_0} from firmware URL")]
    #[from(ignore)]
    OtaHttpStatus(#[error(not(source))] u16),

    /// OTA firmware response lacked a Content-Length header
    #[display("no Content-Length in firmware response")]
    OtaMissingContentLength,

    /// Cache manager mpsc channel was closed
    #[display("cache manager channel closed")]
    CacheManagerChannelClosed,

    /// Failed to register with the cache manager
    #[display("failed to register with cache manager")]
    CacheManagerRegistrationFailed,
}

/// Crate-wide result alias.
///
/// Accepts an optional second type parameter (defaults to [`Error`]) so that
/// code using a different error type (e.g. `Result<T, DongleError>`) keeps
/// compiling without qualifying `std::result::Result`.
pub type Result<T, E = Error> = std::result::Result<T, E>;
