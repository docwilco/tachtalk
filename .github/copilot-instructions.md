## Running Cargo commands

This project has a mixed target setup:
- `tachtalk-firmware` targets ESP32 (xtensa) and must be built from its subdirectory
- All other crates target the host system

## Clippy

To run pedantic clippy on all crates:

```bash
# Non-firmware crates (from workspace root)
cargo clippy --all-targets --all-features --workspace --exclude tachtalk-firmware --exclude tachtalk-test-firmware -- -W clippy::pedantic

# Firmware crates (from their respective directories)
cd tachtalk-firmware && cargo clippy --all-targets --all-features -- -W clippy::pedantic
cd ..
cd tachtalk-test-firmware && cargo clippy --all-targets --all-features -- -W clippy::pedantic
```

## SmallVec

This is an embedded project, so we use `smallvec` where appropriate to avoid heap allocations. If you add a new `Vec`, consider whether it should be `SmallVec` instead. The crate is already included in the workspace. You can add it to your crate by adding the following to your `Cargo.toml`:

```toml
[dependencies]
smallvec = { workspace = true }
```

When using `SmallVec`, you need to specify the inline size (the number of elements that can be stored without heap allocation). The optimal size depends on your use case and the typical number of elements. A common choice is 4 or 8, but you should profile your code to find the best size for your specific use case. With union enabled to compute this in Rust at runtime:

```rust
fn smallvec_size<T>(n: usize) -> usize {
    let ptr_size = std::mem::size_of::<usize>();
    let union_align = std::mem::align_of::<T>().max(ptr_size);
    let union_size = (n * std::mem::size_of::<T>()).max(2 * ptr_size).next_multiple_of(union_align);
    union_align + union_size
}
```

For instance:

```
SmallVec<[u8; N]> sizes: 1=12, 2=12, 3=12, 4=12, 5=12, 6=12, 7=12, 8=12, 9=16, 10=16, 11=16, 12=16, 13=20, 14=20, 15=20, 16=20, 17=24, 18=24, 19=24, 20=24, 21=28, 22=28, 23=28, 24=28, 25=32, 26=32, 27=32, 28=32, 29=36, 30=36, 31=36, 32=36
SmallVec<[u16; N]> sizes: 1=12, 2=12, 3=12, 4=12, 5=16, 6=16, 7=20, 8=20, 9=24, 10=24, 11=28, 12=28, 13=32, 14=32, 15=36, 16=36
SmallVec<[u32; N]> sizes: 1=12, 2=12, 3=16, 4=20, 5=24, 6=28, 7=32, 8=36
SmallVec<[u64; N]> sizes: 1=16, 2=24, 3=32, 4=40
```

These are of course the sizes of the `SmallVec` struct itself, not the total memory usage. The total memory usage will be the size of the struct plus the size of the elements (if heap allocated). So for `SmallVec<[u8; 4]>` with 8 elements, the total memory usage would be 12 (struct) + 8 (elements) = 20 bytes. And for `SmallVec<[u8; 8]>` with 8 elements, the total memory usage would be 12 bytes, since all elements fit in the struct. 

So be mindful of the inline size you choose, as choosing too small may actually be wasting memory due to heap allocations, while choosing too large may waste memory due to the larger struct size.

# Configuration

Anything related to configuration should be added to the Web UI and stored in NVS. The `Config` struct in `tachtalk-firmware/src/config.rs` is the single source of truth for all configuration options. 
