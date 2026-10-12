//! Build-only WDM probe for the Rust/WDK toolchain.
//! No device, audio pins, shared memory, IOCTLs or PCM producer exists yet.
//! Deliberately refuses loading until the real endpoint lifecycle is implemented.
#![no_std]

extern crate wdk_panic;

use wdk_sys::{DRIVER_OBJECT, NTSTATUS, PCUNICODE_STRING, STATUS_NOT_SUPPORTED, ntddk::DbgPrint};

/// Check the WDK entry-point ABI, kernel import linking and shared protocol dependency.
///
/// # Safety
/// Windows provides the pointers according to the DriverEntry ABI. This probe
/// never dereferences them or retains them.
#[unsafe(export_name = "DriverEntry")]
pub unsafe extern "system" fn driver_entry(
    _driver: *mut DRIVER_OBJECT,
    _registry_path: PCUNICODE_STRING,
) -> NTSTATUS {
    // SAFETY: the static nul-terminated format has one integer conversion,
    // matching the promoted unsigned integer argument. No user memory is read.
    unsafe {
        DbgPrint(
            c"Matrix Input build probe: PCM protocol %u; endpoint not implemented.\n"
                .as_ptr()
                .cast(),
            u32::from(matrix_bridge::VERSION),
        );
    }
    STATUS_NOT_SUPPORTED
}
