//! Skarve computes raster statistics for GeoJSON polygons with explicit numerical
//! policies and bounded source access. Start with [`Skarve`], [`Source`] and
//! [`CarveOptions`]. The native grid planar fractional policy is the default.
//!
//! The Cargo package is `skarve`; the library target remains `raster_engine` to
//! preserve existing Rust imports and the C ABI. See `docs/rust.md` for tested
//! external-consumer examples and the supported native dependency environment.
//!
//! Internal modules remain public for compatibility, but the branded facade is
//! the recommended alpha consumer API. Advanced requests retain the same strict
//! validation and resource admission as the Python, Node and CLI interfaces.

#![doc = include_str!("../docs/rust.md")]

pub mod api;
pub use api::{Batch, Cancellation, CarveOptions, Skarve, Source};

pub mod aggregate;
pub mod backend;
pub mod batch;
pub mod bulk;
pub mod coverage;
pub mod cumulative;
pub mod exactextract;
pub mod exactextract_job;
pub mod expression;
pub mod hierarchy;
pub mod hm_compat;
pub mod io;
pub mod joint_plan;
pub mod model;
pub mod ordered_source;
pub mod persistent;
mod precise;
mod range_schedule;
pub mod reducers;
pub mod serving_profile;
pub mod session;
pub mod shared_rows;
pub mod skv;
pub mod source;
pub mod stored_summary;
pub mod streaming;
pub mod tile_cache;

use std::{
    ffi::{CStr, CString, c_char},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
pub struct Handle {
    session: Mutex<session::Session>,
    cancel: AtomicBool,
    cancel_generation: AtomicU64,
}

#[unsafe(no_mangle)]
pub extern "C" fn re_new() -> *mut Handle {
    Box::into_raw(Box::new(Handle {
        session: Mutex::new(session::Session::default()),
        cancel: AtomicBool::new(false),
        cancel_generation: AtomicU64::new(0),
    }))
}

/// # Safety
/// handle must be a live re_new handle; input must be a valid NUL-terminated
/// string for the duration of this call. No concurrent re_drop is permitted.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn re_call(handle: *mut Handle, input: *const c_char) -> *mut c_char {
    let result = std::panic::catch_unwind(|| {
        if handle.is_null() || input.is_null() {
            return "{\"ok\":false,\"error\":\"null handle/input\"}".to_string();
        }
        let h = unsafe { &*handle };
        let text = unsafe { CStr::from_ptr(input) }.to_str();
        let mut session = match h.session.try_lock() {
            Ok(s) => s,
            Err(_) => return "{\"ok\":false,\"error\":\"session busy or poisoned\"}".to_string(),
        };
        // cancellation remains sticky until the next completed call; only reset
        // before starting while session ownership prevents concurrent work.
        h.cancel.store(false, Ordering::Relaxed);
        match text {
            Ok(s) => session::request(&mut session, s, &h.cancel),
            Err(_) => "{\"ok\":false,\"error\":\"input is not UTF-8\"}".to_string(),
        }
    })
    .unwrap_or_else(|_| "{\"ok\":false,\"error\":\"native panic contained\"}".to_string());
    CString::new(result)
        .expect("JSON contains no NUL")
        .into_raw()
}
/// # Safety
/// handle must remain live until all re_call/re_cancel invocations complete.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn re_cancel(handle: *mut Handle) {
    if !handle.is_null() {
        unsafe { &*handle }
            .cancel_generation
            .fetch_add(1, Ordering::AcqRel);
        unsafe { &*handle }.cancel.store(true, Ordering::Relaxed);
    }
}
/// # Safety
/// ptr must be an unfreed pointer returned by re_call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn re_free_string(ptr: *mut c_char) {
    if !ptr.is_null() {
        drop(unsafe { CString::from_raw(ptr) });
    }
}
/// # Safety
/// handle must be from re_new, not freed and have no calls in progress.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn re_drop(handle: *mut Handle) {
    if !handle.is_null() {
        drop(unsafe { Box::from_raw(handle) });
    }
}
