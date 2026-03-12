# Security Hardening Plan

## Completed

### Phase 2: Session-Based Web Authentication

- [x] PBKDF2-HMAC-SHA256 password hashing via mbedTLS (`auth.rs`)
- [x] In-memory session store with 128-bit random tokens (max 5 sessions, FIFO eviction)
- [x] `cookie` crate for RFC 6265 compliant `HttpOnly` session cookies
- [x] `hex` crate for encode/decode
- [x] `is_authenticated()` helper checks Cookie header; returns `true` when no password is set
- [x] Auth endpoints: `GET /api/auth/status`, `POST /api/auth/login`, `POST /api/auth/logout`, `POST /api/auth/set-password`
- [x] 13 admin endpoints protected with auth checks
- [x] Config redaction: Wi-Fi/AP passwords masked for unauthenticated users
- [x] Admin password hash stored in separate NVS key (`admin_pw`), not in Config struct
- [x] Clean build + zero pedantic clippy warnings

## In Progress

### Phase 3: Replace `EspHttpServer` with `edge-http` and Unify Services

**Motivation**: `EspHttpServer` (esp-idf-svc wrapper around ESP-IDF httpd) binds to `0.0.0.0` with no way to restrict to a specific interface. The wrapper hardcodes `open_fn: None` in its `httpd_config_t` and doesn't expose it in the `Configuration` struct. Rather than hack around this (`unsafe` to poke internal C structs, or per-handler `getsockname()` checks), replace the HTTP stack entirely with `edge-http` — which gives us direct control over the `TcpListener::bind()` address and unlocks several downstream improvements.

**Key benefits**:
- **AP-only binding**: `TcpListener::bind((ap_ip, 443))` — trivial, no hacks
- **Unified SSE**: SSE moves from a separate `TcpListener` on port 81 to the same server, routed by path — eliminates one socket, simplifies client code (no `SSE_PORT`)
- **Native TLS path**: `edge-http` works with `esp-mbedtls` socket wrapping ([HTTPS example](https://github.com/esp-rs/esp-mbedtls/blob/main/examples/edge_server.rs)), enabling Phase 4 naturally
- **Async concurrency**: `smol` executor with per-connection `spawn` — no artificial 4-handler slot limit from `Server::run`

**Architecture**:
```
smol::block_on(async {
    let listener = smol::Async::<TcpListener>::bind((ap_ip, 443))?;
    loop {
        let (stream, peer) = listener.accept().await?;
        let tls_stream = wrap_tls(stream).await?;
        smol::spawn(handle_connection(tls_stream, state.clone())).detach();
    }
})
```

Each connection is an async task. The handler dispatches by method+path:
- SSE requests (`GET /api/events`) hold the connection open with an async write loop
- Normal API requests are processed and completed (HTTP keep-alive handled by `edge-http`)
- Captive portal wildcard is the fallback route

**New dependencies**:
- `edge-http` — async no_std HTTP server/client (~3.5K SLoC, `httparse`-based)
- `edge-nal-std` — std networking abstraction (provides `TcpBind`, `TcpConnect` over `std::net`)
- `smol` — lightweight async executor (or `futures-lite` + manual executor)
- `esp-mbedtls` — TLS socket wrapper for ESP-IDF (Phase 4)

**Removed dependencies**:
- `esp-idf-svc::http::server::EspHttpServer` (and its `embedded-svc` HTTP traits)

#### Step 3.1: Add `smol` + `edge-http` + `edge-nal-std`

- [ ] Add crates to `Cargo.toml` (workspace + firmware)
- [ ] Verify compilation on xtensa target
- [ ] Scaffold minimal async server that binds to AP IP and returns 200 OK

#### Step 3.2: Port HTTP handlers

Port all 29 handlers from synchronous `EspHttpServer` closures to async functions using `edge-http`'s `Connection` API.

Current handler inventory (29 total):

**Auth routes** (4 handlers):
| Route | Method | Auth | Description |
|-------|--------|------|-------------|
| `/api/auth/status` | GET | No | Auth state query |
| `/api/auth/login` | POST | No | Password verify, issue session cookie |
| `/api/auth/logout` | POST | No | Clear session, invalidate cookie |
| `/api/auth/set-password` | POST | Admin | Set/change/disable admin password |

**Config routes** (6 handlers):
| Route | Method | Auth | Description |
|-------|--------|------|-------------|
| `/` | GET | No | Serve `index.html` with SSE port injection |
| `/api/config` | GET | Conditional | Full config if authed, redacted otherwise |
| `/api/config/default` | GET | No | Default config template |
| `/api/config/check` | POST | Admin | Validate config, check restart needed |
| `/api/config` | POST | Admin | Update and persist config |
| `/api/brightness` | POST | Admin | Immediate LED brightness update |

**Network routes** (2 handlers):
| Route | Method | Auth | Description |
|-------|--------|------|-------------|
| `/api/wifi/scan` | GET | No | Scan Wi-Fi networks |
| `/api/network` | GET | No | Current network status |

**Status routes** (4 handlers):
| Route | Method | Auth | Description |
|-------|--------|------|-------------|
| `/api/status` | GET | No | Connection status diagram data |
| `/api/tcp` | GET | No | TCP connection details |
| `/api/rpm` | GET | No | Current RPM (non-SSE fallback) |
| `/api/metrics` | GET | No | Polling metrics |

**Debug routes** (3 handlers):
| Route | Method | Auth | Description |
|-------|--------|------|-------------|
| `/api/sockets` | GET | Admin | Enumerate LWIP sockets |
| `/api/debug_info` | GET | Admin | Memory stats, AT commands, PIDs |
| `/api/reboot` | POST | Admin | Reboot device |

**Capture routes** (5 handlers):
| Route | Method | Auth | Description |
|-------|--------|------|-------------|
| `/api/capture` | GET | Admin | Download capture binary |
| `/api/capture/clear` | POST | Admin | Clear capture buffer |
| `/api/capture/status` | GET | No | Capture buffer status |
| `/api/capture/start` | POST | Admin | Start capture |
| `/api/capture/stop` | POST | Admin | Stop capture |

**OTA routes** (4 handlers):
| Route | Method | Auth | Description |
|-------|--------|------|-------------|
| `/api/ota/info` | GET | No | Firmware version info |
| `/api/ota/upload` | POST | Admin | Upload firmware binary |
| `/api/ota/download` | POST | Admin | Download firmware from URL |
| `/api/ota/status` | GET | No | OTA download progress |

**Captive portal** (1 handler):
| Route | Method | Auth | Description |
|-------|--------|------|-------------|
| `/*` | GET | No | Wildcard redirect (or 404 for valid hosts) |

Porting approach:
- [ ] Create async router function that dispatches `(method, path)` → handler
- [ ] Implement auth middleware as a wrapper that checks `is_authenticated()` before calling the inner handler
- [ ] Port handlers group by group, starting with simplest (status routes) and ending with complex (OTA upload with streaming)
- [ ] Replace `req.header()` with `headers.get()`, `req.into_response()` with `conn.initiate_response()`, etc.
- [ ] Handle request body reading via `conn.read()` (async) instead of sync `Read` trait

#### Step 3.3: Merge SSE into unified server

- [ ] Remove `sse_server.rs` as a separate `TcpListener` on port 81
- [ ] Add `GET /api/events` route that sends `Content-Type: text/event-stream`, then loops writing SSE events
- [ ] SSE handler receives events through the existing `SseMessage` channel (or shared async broadcast)
- [ ] Consider switching `std::sync::mpsc` → async channel (e.g., `smol::channel` or `async-broadcast`)
- [ ] Remove `SSE_PORT` constant and port references from `index.html`
- [ ] Update `index.html` to connect `EventSource` to `/api/events` on same origin

#### Step 3.4: Bind all services to AP IP

- [ ] HTTP/HTTPS server: `TcpListener::bind((ap_ip, 443))` (or port 80 until TLS is added)
- [ ] DNS server: change `UdpSocket::bind(("0.0.0.0", 53))` → `UdpSocket::bind((ap_ip, 53))`
- [ ] OBD2 proxy: change `TcpListener::bind(format!("0.0.0.0:{port}"))` → `TcpListener::bind((ap_ip, port))`
- [ ] Verify all services reject connections from STA interface

#### Step 3.5: Threading model changes

Current threading model (all synchronous, one OS thread per task):
- `main` thread → `web_server::start_server()` (blocks with `mem::forget`)
- `sse_srv` thread (6 KB stack, PSRAM) → `sse_server_task()`
- `dongle` thread (8 KB stack, internal) → `dongle_task()`
- `cache_mgr` thread (6 KB stack, internal) → `cache_manager_task()`
- `rpm_led` thread (4 KB stack, internal, Core 1) → `rpm_led_task()`
- `obd2_proxy` thread (4 KB stack, internal) → `Obd2Proxy::run()`
- `wifi_mgr` thread (4 KB stack, PSRAM) → `wifi_connection_manager()`
- `status_led` thread (3 KB stack, internal) → `run_status_led_task()`
- `controls` thread (6 KB stack, internal) → `controls_task()`
- `dns` thread (spawned in `start_dns_server()`)

After migration:
- `main` thread → `smol::block_on()` running the async HTTP server accept loop
  - Spawns per-connection async tasks (SSE connections included, no separate thread)
- `sse_srv` thread → **removed** (SSE handled as async tasks within HTTP server)
- All other threads remain unchanged (dongle, cache_mgr, rpm_led, obd2_proxy, wifi_mgr, status_led, controls, dns)

Net result: one fewer OS thread, SSE connections are cheap async tasks instead of raw `TcpStream` management.

### Phase 4: HTTPS with Self-Signed Certificate

- [ ] Generate EC P-256 key pair on first boot using mbedTLS
- [ ] Create self-signed X.509 certificate with AP IP as Subject Alternative Name
- [ ] Store private key and certificate in NVS
- [ ] Regenerate certificate when AP IP changes (detect on config change)
- [ ] Wrap accepted TCP sockets with `esp-mbedtls` TLS before passing to `edge-http` `Connection::new()`
- [ ] Use PSRAM for TLS session buffers (TLS needs ~40 KB per connection)
- [ ] Redirect HTTP (port 80) → HTTPS (port 443)
- [ ] Update `index.html` and any hardcoded `http://` references

### Phase 5: AP Password Warning

- [ ] Add warning banner in `index.html` when AP is using default/no password
- [ ] Check `ap_password` field in config — warn if empty, short, or matches a known default
- [ ] Banner should be dismissible but re-appear on page reload until password is changed
- [ ] Style as a prominent security warning (red/orange banner at top of page)
