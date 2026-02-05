## Running Cargo commands

This project has a mixed target setup:
- `tachtalk-firmware` targets ESP32 (xtensa) and must be built from its subdirectory
- All other crates target the host system

## Clippy

To run pedantic clippy on all crates:

```bash
# Non-firmware crates (from workspace root)
cargo clippy --all-targets --all-features --workspace --exclude tachtalk-firmware -- -W clippy::pedantic

# Firmware crate (from tachtalk-firmware directory)
cd tachtalk-firmware && cargo clippy --all-targets --all-features -- -W clippy::pedantic
```
