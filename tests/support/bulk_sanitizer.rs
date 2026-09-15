//! Standalone AddressSanitizer harness. Compile via scripts/check_bulk_asan.sh.
//! The current bulk.rs and all ABI integration tests are instrumented. The
//! unchanged model::Sum comes from the normal debug rlib; it is not instrumented.
//! Minimal handle ownership below mirrors lib.rs so this target needs no second
//! compilation of GDAL/network dependencies. The real Handle is separately
//! covered by cargo test --lib bulk::tests.
extern crate control_core;
extern crate self as raster_engine;
use std::sync::{Mutex, atomic::{AtomicBool, AtomicU64, Ordering}};
pub mod model { pub use control_core::model::Sum; }
pub struct Handle {
    session: Mutex<()>,
    cancel: AtomicBool,
    cancel_generation: AtomicU64,
}
pub fn re_new() -> *mut Handle {
    Box::into_raw(Box::new(Handle { session: Mutex::new(()), cancel: AtomicBool::new(false),
                                  cancel_generation: AtomicU64::new(0) }))
}
/// # Safety
/// Live handle; no concurrent deallocation.
pub unsafe fn re_cancel(handle: *mut Handle) {
    if !handle.is_null() {
        unsafe { &*handle }.cancel_generation.fetch_add(1, Ordering::AcqRel);
        unsafe { &*handle }.cancel.store(true, Ordering::Relaxed);
    }
}
/// # Safety
/// Live exclusively owned handle; no active calls.
pub unsafe fn re_drop(handle: *mut Handle) {
    if !handle.is_null() { drop(unsafe { Box::from_raw(handle) }); }
}
#[path = "../../src/bulk.rs"]
pub mod bulk;
#[path = "../bulk_abi.rs"]
mod abi_tests;

