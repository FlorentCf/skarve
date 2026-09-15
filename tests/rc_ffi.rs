//! Bounded deterministic malformed-input exercise of the public C ABI.
//! This respects the documented pointer lifetime contract; it is not a claim
//! that arbitrary dangling pointers from foreign callers can be made safe.
use raster_engine::{re_call, re_cancel, re_drop, re_free_string, re_new};
use std::ffi::{CStr, CString};

#[test]
fn malformed_utf8_json_and_protocol_leave_ffi_handle_usable() {
    let handle = re_new();
    assert!(!handle.is_null());
    let mut seed = 0x71e5_912c_f401_7843_u64;
    for i in 0..2048 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let mut request = match i % 6 {
            0 => format!(r#"{{"op":"open","id":"r","raster":{{"grid":{{"width":{},"height":18446744073709551615}},"bands":[]}}}}"#, seed),
            1 => format!(r#"{{"op":"register_index","id":"i","source":"missing","index":"invalid-{}"}}"#, seed),
            2 => format!(r#"{{"op":"measure","source":"missing","geometry":{},"crs":"LOCAL"}}"#, seed),
            3 => "[".repeat(140) + "0" + &"]".repeat(140),
            4 => format!(r#"{{"op":"stats","unrecognized":{}}}"#, seed),
            _ => format!("{{bad-json-{seed}"),
        }.into_bytes();
        if i % 11 == 0 {
            request.push(0xff);
        }
        let input = CString::new(request).unwrap();
        // SAFETY: handle is live, input is NUL-terminated for this call, each
        // returned string is copied before exactly one corresponding free.
        unsafe {
            let output = re_call(handle, input.as_ptr());
            assert!(!output.is_null());
            let parsed: serde_json::Value =
                serde_json::from_slice(CStr::from_ptr(output).to_bytes()).unwrap();
            re_free_string(output);
            assert_eq!(parsed["ok"], false);
        }
    }
    unsafe {
        re_cancel(handle);
        let output = re_call(handle, c"{\"op\":\"stats\"}".as_ptr());
        let parsed: serde_json::Value =
            serde_json::from_slice(CStr::from_ptr(output).to_bytes()).unwrap();
        re_free_string(output);
        assert_eq!(parsed["ok"], true);
        let null_error = re_call(std::ptr::null_mut(), c"{}".as_ptr());
        let parsed: serde_json::Value =
            serde_json::from_slice(CStr::from_ptr(null_error).to_bytes()).unwrap();
        re_free_string(null_error);
        assert_eq!(parsed["ok"], false);
        re_drop(handle);
        re_free_string(std::ptr::null_mut());
        re_drop(std::ptr::null_mut());
    }
}
