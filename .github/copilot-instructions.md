## Running Cargo commands

This project has a mixed target setup:
- `tachtalk-firmware` targets ESP32 (xtensa) and must be built from its subdirectory
- All other crates target the host system

## Clippy

To run pedantic clippy on all crates:

```bash
# Non-firmware crates (from workspace root)
cargo clippy --all-targets --all-features --workspace --exclude tachtalk-firmware --exclude tachtalk-test-firmware -- -W clippy::pedantic

# Firmware crate (from tachtalk-firmware directory)
cd tachtalk-firmware && cargo clippy --all-targets --all-features -- -W clippy::pedantic
```

## SmallVec

This is an embedded project, so we use `smallvec` where appropriate to avoid heap allocations. If you add a new dependency, consider whether it should be added to `smallvec` instead of `Vec`. A Vec on this platform will be 12 bytes (pointer + length + capacity) and will require heap allocation on top of that. The crate is already included in the workspace. You can add it to your crate by adding the following to your `Cargo.toml`:

```toml
[dependencies]
smallvec = { workspace = true }
```

# Configuration

Anything related to configuration should be added to the Web UI and stored in NVS. The `Config` struct in `tachtalk-firmware/src/config.rs` is the single source of truth for all configuration options. 
