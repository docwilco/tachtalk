//! Authentication and session management.
//!
//! Passwords are hashed with PBKDF2-HMAC-SHA256 (via mbedTLS) and stored in
//! NVS as `"iterations:salt_hex:hash_hex"`. Sessions are in-memory random
//! tokens issued on successful login and returned as `HttpOnly` cookies.

use esp_idf_svc::sys::{
    esp_fill_random, mbedtls_md_type_t_MBEDTLS_MD_SHA256, mbedtls_pkcs5_pbkdf2_hmac_ext,
};
use log::warn;
use std::sync::Mutex;

/// PBKDF2 iteration count — balances brute-force resistance with ESP32-S3
/// response time (~200-400ms per hash at 10 000 iterations).
const PBKDF2_ITERATIONS: u32 = 10_000;

/// Salt length in bytes.
const SALT_LEN: usize = 16;

/// Derived key length in bytes (SHA-256 output).
const HASH_LEN: usize = 32;

/// Maximum number of concurrent sessions.
const MAX_SESSIONS: usize = 5;

/// Session token length in bytes (128-bit).
const TOKEN_LEN: usize = 16;

/// In-memory session store.
#[derive(Default)]
pub struct SessionStore {
    tokens: Mutex<Vec<[u8; TOKEN_LEN]>>,
}

impl SessionStore {
    /// Insert a new session token. Evicts the oldest if at capacity.
    pub fn insert(&self, token: [u8; TOKEN_LEN]) {
        let mut tokens_guard = self.tokens.lock().unwrap();
        if tokens_guard.len() >= MAX_SESSIONS {
            tokens_guard.remove(0);
        }
        tokens_guard.push(token);
    }

    /// Check whether a token is valid.
    pub fn contains(&self, token: &[u8; TOKEN_LEN]) -> bool {
        self.tokens.lock().unwrap().contains(token)
    }

    /// Remove a token (logout).
    pub fn remove(&self, token: &[u8; TOKEN_LEN]) {
        let mut tokens_guard = self.tokens.lock().unwrap();
        tokens_guard.retain(|t| t != token);
    }

    /// Remove all sessions (e.g. when auth is disabled).
    pub fn clear(&self) {
        self.tokens.lock().unwrap().clear();
    }
}

/// Generate `len` bytes of cryptographic random data using the ESP hardware RNG.
fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    unsafe {
        esp_fill_random(buf.as_mut_ptr().cast(), N);
    }
    buf
}

/// Generate a new random session token.
pub fn generate_session_token() -> [u8; TOKEN_LEN] {
    random_bytes::<TOKEN_LEN>()
}

/// Compute PBKDF2-HMAC-SHA256 hash of `password` with the given `salt` and
/// `iterations`. Returns the derived key on success.
fn pbkdf2(password: &[u8], salt: &[u8], iterations: u32) -> Result<[u8; HASH_LEN], i32> {
    let mut output = [0u8; HASH_LEN];
    #[allow(clippy::cast_possible_truncation)]
    let ret = unsafe {
        mbedtls_pkcs5_pbkdf2_hmac_ext(
            mbedtls_md_type_t_MBEDTLS_MD_SHA256,
            password.as_ptr(),
            password.len(),
            salt.as_ptr(),
            salt.len(),
            iterations,
            HASH_LEN as u32,
            output.as_mut_ptr(),
        )
    };
    if ret != 0 {
        warn!("PBKDF2 failed: mbedtls error {ret}");
        return Err(ret);
    }
    Ok(output)
}

/// Hash a password for storage. Returns the formatted string
/// `"iterations:salt_hex:hash_hex"`.
pub fn hash_password(password: &str) -> Result<String, i32> {
    let salt = random_bytes::<SALT_LEN>();
    let hash = pbkdf2(password.as_bytes(), &salt, PBKDF2_ITERATIONS)?;
    Ok(format!(
        "{PBKDF2_ITERATIONS}:{}:{}",
        hex::encode(salt),
        hex::encode(hash),
    ))
}

/// Verify a password against a stored hash string (`"iterations:salt_hex:hash_hex"`).
pub fn verify_password(password: &str, stored: &str) -> bool {
    let parts: Vec<&str> = stored.splitn(3, ':').collect();
    if parts.len() != 3 {
        warn!("Invalid password hash format");
        return false;
    }
    let Some(iterations) = parts[0].parse::<u32>().ok() else {
        warn!("Invalid iteration count in password hash");
        return false;
    };
    let Some(salt) = hex::decode(parts[1]).ok() else {
        warn!("Invalid salt in password hash");
        return false;
    };
    let Some(expected_hash) = hex::decode(parts[2]).ok() else {
        warn!("Invalid hash in password hash");
        return false;
    };

    let Ok(computed) = pbkdf2(password.as_bytes(), &salt, iterations) else {
        return false;
    };

    // Constant-time comparison to prevent timing attacks
    constant_time_eq(&computed, &expected_hash)
}

/// Constant-time byte comparison.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Extract a session token from the `Cookie` header.
///
/// Looks for the `session` cookie containing a hex-encoded token.
pub fn extract_session_token(cookie_header: &str) -> Option<[u8; TOKEN_LEN]> {
    // Parse all cookies from the header using the cookie crate
    for pair in cookie_header.split(';') {
        if let Ok(c) = cookie::Cookie::parse(pair.trim()) {
            if c.name() == "session" {
                if let Ok(bytes) = hex::decode(c.value()) {
                    if bytes.len() == TOKEN_LEN {
                        let mut token = [0u8; TOKEN_LEN];
                        token.copy_from_slice(&bytes);
                        return Some(token);
                    }
                }
            }
        }
    }
    None
}

/// Build a `Set-Cookie` header value for a session token.
pub fn session_cookie(token: &[u8; TOKEN_LEN]) -> String {
    let mut c = cookie::Cookie::new("session", hex::encode(token));
    c.set_path("/");
    c.set_http_only(true);
    c.set_same_site(cookie::SameSite::Strict);
    c.to_string()
}

/// Build a `Set-Cookie` header value that clears the session cookie.
pub fn clear_session_cookie() -> String {
    let mut c = cookie::Cookie::new("session", "");
    c.set_path("/");
    c.set_http_only(true);
    c.set_same_site(cookie::SameSite::Strict);
    c.set_max_age(cookie::time::Duration::ZERO);
    c.to_string()
}
