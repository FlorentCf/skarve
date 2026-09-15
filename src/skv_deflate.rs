//! Bounded system libdeflate decoding for existing SKV zlib packets.
//! Reviewed API: https://raw.githubusercontent.com/ebiggers/libdeflate/v1.19/libdeflate.h
//! Allocation: https://raw.githubusercontent.com/ebiggers/libdeflate/v1.19/lib/deflate_decompress.c
//! Zlib checks: https://raw.githubusercontent.com/ebiggers/libdeflate/v1.19/lib/zlib_decompress.c
//! No upstream implementation is copied. Encoder and SKV bytes remain unchanged.
#![cfg(target_os = "linux")]
use crate::model::check_cancel;
use anyhow::{Context, Result, ensure};
use std::{
    alloc::{Layout, alloc, dealloc},
    cell::Cell,
    ffi::{c_int, c_void},
    marker::PhantomData,
    ptr::{self, NonNull},
    rc::Rc,
    sync::atomic::AtomicBool,
};

pub(super) const CONTEXT_BOUND: usize = 64 << 10;
const MAX_BLOCK: usize = 4 << 20;

#[repr(C)]
struct Options {
    sizeof_options: usize,
    malloc_func: Option<unsafe extern "C" fn(usize) -> *mut c_void>,
    free_func: Option<unsafe extern "C" fn(*mut c_void)>,
}

// GNU/Linux can link the installed runtime soname without a development symlink.
// The qualified Linux runtime requires the per-context v1.19 API symbols.
#[link(name = "libdeflate.so.0", kind = "dylib", modifiers = "+verbatim")]
unsafe extern "C" {
    fn libdeflate_alloc_decompressor_ex(options: *const Options) -> *mut c_void;
    fn libdeflate_free_decompressor(decompressor: *mut c_void);
    fn libdeflate_zlib_decompress_ex(
        decompressor: *mut c_void,
        input: *const c_void,
        input_bytes: usize,
        output: *mut c_void,
        output_bytes: usize,
        actual_input: *mut usize,
        actual_output: *mut usize,
    ) -> c_int;
}

#[derive(Clone, Copy)]
struct State {
    active: bool,
    limit: usize,
    live: usize,
    peak: usize,
    invalid: bool,
}
const IDLE: State = State {
    active: false,
    limit: 0,
    live: 0,
    peak: 0,
    invalid: false,
};
thread_local! {
    // The C API has no allocator userdata. Its callbacks are synchronous on the
    // allocating/calling thread; a scoped, non-Send decoder owns this state.
    static STATE: Cell<State> = const { Cell::new(IDLE) };
}

// Includes accounting in the hard aggregate cap. Sixteen-byte alignment meets
// the reviewed Linux C context's fundamental alignment; the prefix preserves it.
#[repr(C, align(16))]
struct Prefix {
    charge: usize,
}
const PREFIX: usize = std::mem::size_of::<Prefix>();
const ALIGN: usize = std::mem::align_of::<Prefix>();

unsafe extern "C" fn bounded_alloc(requested: usize) -> *mut c_void {
    let Some(charge) = requested.max(1).checked_add(PREFIX) else {
        return ptr::null_mut();
    };
    let Ok(layout) = Layout::from_size_align(charge, ALIGN) else {
        return ptr::null_mut();
    };
    let admitted = STATE
        .try_with(|cell| {
            let mut state = cell.get();
            let Some(next) = state.live.checked_add(charge) else {
                return false;
            };
            if !state.active || next > state.limit {
                return false;
            }
            state.live = next;
            state.peak = state.peak.max(next);
            cell.set(state);
            true
        })
        .unwrap_or(false);
    if !admitted {
        return ptr::null_mut();
    }
    // SAFETY: checked nonzero Layout, with fundamental C alignment. Null is a
    // valid allocation failure and is returned to C, never dereferenced.
    let base = unsafe { alloc(layout) };
    if base.is_null() {
        release_charge(charge);
        return ptr::null_mut();
    }
    // SAFETY: allocation includes the aligned prefix and requested payload.
    unsafe {
        base.cast::<Prefix>().write(Prefix { charge });
        base.add(PREFIX).cast()
    }
}
fn release_charge(charge: usize) {
    let _ = STATE.try_with(|cell| {
        let mut state = cell.get();
        match state.live.checked_sub(charge) {
            Some(live) if state.active => state.live = live,
            _ => {
                state.invalid = true;
                state.live = 0;
            }
        }
        cell.set(state);
    });
}
unsafe extern "C" fn bounded_free(pointer: *mut c_void) {
    if pointer.is_null() {
        return;
    }
    // SAFETY: the per-context upstream free function receives exactly the
    // payload pointer returned by bounded_alloc. The context is not exposed.
    let base = unsafe { pointer.cast::<u8>().sub(PREFIX) };
    let charge = unsafe { base.cast::<Prefix>().read().charge };
    // The stored Layout is valid by construction; avoid panics in C callbacks.
    if let Ok(layout) = Layout::from_size_align(charge, ALIGN) {
        unsafe {
            dealloc(base, layout);
        }
        release_charge(charge);
    } else {
        let _ = STATE.try_with(|cell| {
            let mut state = cell.get();
            state.invalid = true;
            cell.set(state);
        });
    }
}

struct BudgetScope {
    finished: bool,
    _same_thread: PhantomData<Rc<()>>,
}
impl BudgetScope {
    fn enter(limit: usize) -> Result<Self> {
        ensure!(
            limit > 0 && limit <= CONTEXT_BOUND,
            "invalid SKV decoder context budget"
        );
        let admitted = STATE
            .try_with(|cell| {
                if cell.get().active {
                    return false;
                }
                cell.set(State {
                    active: true,
                    limit,
                    ..IDLE
                });
                true
            })
            .unwrap_or(false);
        ensure!(admitted, "SKV decoder allocator unavailable or reentrant");
        Ok(Self {
            finished: false,
            _same_thread: PhantomData,
        })
    }
    fn finish(mut self) -> Result<usize> {
        let state = STATE
            .try_with(|cell| {
                let state = cell.get();
                cell.set(IDLE);
                state
            })
            .context("SKV decoder allocation accounting unavailable")?;
        self.finished = true;
        ensure!(
            state.active && state.live == 0 && !state.invalid,
            "SKV decoder context did not release its bounded allocation"
        );
        Ok(state.peak)
    }
}
impl Drop for BudgetScope {
    fn drop(&mut self) {
        if !self.finished {
            let _ = STATE.try_with(|cell| cell.set(IDLE));
        }
    }
}
struct Decompressor {
    pointer: NonNull<c_void>,
    _same_thread: PhantomData<Rc<()>>,
}
impl Drop for Decompressor {
    fn drop(&mut self) {
        // SAFETY: allocated by the matching API, alive, uniquely owned and
        // freed once on the allocator scope's original thread.
        unsafe {
            libdeflate_free_decompressor(self.pointer.as_ptr());
        }
    }
}

pub(super) struct Decoded {
    pub bytes: Vec<u8>,
    pub context_peak_bytes: usize,
}

pub(super) fn decode_zlib(input: &[u8], expected: usize, cancel: &AtomicBool) -> Result<Decoded> {
    decode_with_limit(input, expected, cancel, CONTEXT_BOUND)
}
fn decode_with_limit(
    input: &[u8],
    expected: usize,
    cancel: &AtomicBool,
    limit: usize,
) -> Result<Decoded> {
    check_cancel(cancel)?;
    ensure!(
        !input.is_empty() && input.len() <= MAX_BLOCK && expected <= MAX_BLOCK,
        "SKV codec input/output exceeds bounded block size"
    );
    // Declaration order guarantees the native context drops before its budget,
    // including every early error. Nothing installs a process-global allocator.
    let budget = BudgetScope::enter(limit)?;
    let options = Options {
        sizeof_options: std::mem::size_of::<Options>(),
        malloc_func: Some(bounded_alloc),
        free_func: Some(bounded_free),
    };
    // SAFETY: repr(C) matches reviewed ABI; callbacks and options outlive alloc.
    let pointer = NonNull::new(unsafe { libdeflate_alloc_decompressor_ex(&options) })
        .context("SKV libdeflate context allocation failed within its 64 KiB cap")?;
    let context = Decompressor {
        pointer,
        _same_thread: PhantomData,
    };
    let mut output = Vec::new();
    output
        .try_reserve_exact(expected.max(1))
        .context("SKV decoder output allocation failed")?;
    output.resize(expected, 0);
    check_cancel(cancel)?;
    let (mut consumed, mut written) = (0usize, 0usize);
    // SAFETY: disjoint slices with explicit lengths, initialized output of exact
    // capacity, valid exclusive context and live size_t counters. C may leave
    // failed output undefined; no failed output is returned or interpreted.
    let status = unsafe {
        libdeflate_zlib_decompress_ex(
            context.pointer.as_ptr(),
            input.as_ptr().cast(),
            input.len(),
            output.as_mut_ptr().cast(),
            output.len(),
            &mut consumed,
            &mut written,
        )
    };
    // The full-buffer native call is the cancellation unit (at most4MiB).
    check_cancel(cancel)?;
    ensure!(
        status == 0,
        "invalid SKV zlib stream (libdeflate status {status})"
    );
    ensure!(consumed == input.len(), "SKV compressed trailing data");
    ensure!(written == expected, "SKV decoded length mismatch");
    drop(context);
    let context_peak_bytes = budget.finish()?;
    Ok(Decoded {
        bytes: output,
        context_peak_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::ZlibEncoder};
    use std::{
        io::Write,
        sync::{Arc, Barrier},
    };
    fn encoded(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(3));
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }
    fn hex(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }
    #[test]
    fn stored_static_dynamic_streams_match_existing_decoder() {
        // Independently generated with system zlib, not the SKV encoder.
        let fixtures = [
            (
                0,
                hex("7801011200edff534b562073746f72656420626c6f636b00ff3c0e06d0"),
                b"SKV stored block\0\xff".to_vec(),
            ),
            (
                1,
                hex("78010bce4e2c2a4b5548cbac484d5148cac94fce66f81f4c811800e0601f19"),
                b"Skarve fixed block\0\xff".repeat(4),
            ),
            (
                2,
                hex(
                    "789cedc181000000008020b6fda516a90a0000000000000000000000000000000000000000000000000000000000000018a81681e1",
                ),
                vec![b'A'; 32_768],
            ),
        ];
        for (block_type, input, raw) in fixtures {
            assert_eq!((input[2] >> 1) & 3, block_type);
            let cancel = AtomicBool::new(false);
            assert_eq!(
                super::super::decode_miniz(&input, raw.len(), &cancel).unwrap(),
                raw
            );
            assert_eq!(decode_zlib(&input, raw.len(), &cancel).unwrap().bytes, raw);
        }
    }
    #[test]
    fn unsupported_dictionary_and_invalid_zlib_headers_reject() {
        let raw = b"SKV wrapper contract";
        let input = encoded(raw);
        let cancel = AtomicBool::new(false);
        // 0x7820 satisfies FCHECK and sets FDICT; supply a complete DICTID.
        let mut dictionary = vec![0x78, 0x20, 0, 0, 0, 1];
        dictionary.extend_from_slice(&input[2..]);
        assert_eq!(u16::from_be_bytes([dictionary[0], dictionary[1]]) % 31, 0);
        assert!(decode_zlib(&dictionary, raw.len(), &cancel).is_err());
        for cmf in [0x79u8, 0x88] {
            // wrong method; oversized window
            let flg = (0..=255u8)
                .find(|&flg| flg & 0x20 == 0 && u16::from_be_bytes([cmf, flg]) % 31 == 0)
                .unwrap();
            let mut bad = input.clone();
            bad[..2].copy_from_slice(&[cmf, flg]);
            assert!(decode_zlib(&bad, raw.len(), &cancel).is_err());
        }
        let mut bad_check = input.clone();
        bad_check[1] ^= 1;
        assert_ne!(u16::from_be_bytes([bad_check[0], bad_check[1]]) % 31, 0);
        assert!(decode_zlib(&bad_check, raw.len(), &cancel).is_err());
    }
    #[test]
    fn exact_bounded_zlib_output_and_fresh_contexts() {
        for n in [0, 1, 65_536, 3_276_800, MAX_BLOCK] {
            let raw = (0..n)
                .map(|i| ((i * 73 + (i >> 7)) & 255) as u8)
                .collect::<Vec<_>>();
            let input = encoded(&raw);
            for _ in 0..2 {
                let decoded = decode_zlib(&input, raw.len(), &AtomicBool::new(false)).unwrap();
                assert_eq!(decoded.bytes, raw);
                assert!(
                    decoded.context_peak_bytes > 0 && decoded.context_peak_bytes <= CONTEXT_BOUND
                );
                assert!(!STATE.with(|s| s.get().active));
            }
        }
    }
    #[test]
    fn corrupt_short_trailing_concatenated_and_cancelled_results_never_escape() {
        let raw = vec![9; 65_537];
        let input = encoded(&raw);
        let cancel = AtomicBool::new(false);
        for n in [0, 1, input.len() - 1] {
            assert!(decode_zlib(&input[..n], raw.len(), &cancel).is_err());
        }
        let mut trailing = input.clone();
        trailing.push(0);
        assert!(decode_zlib(&trailing, raw.len(), &cancel).is_err());
        let mut joined = input.clone();
        joined.extend_from_slice(&input);
        assert!(decode_zlib(&joined, raw.len(), &cancel).is_err());
        let mut corrupt = input.clone();
        *corrupt.last_mut().unwrap() ^= 1;
        assert!(decode_zlib(&corrupt, raw.len(), &cancel).is_err());
        assert!(decode_zlib(&input, raw.len() - 1, &cancel).is_err());
        assert!(decode_zlib(&input, raw.len() + 1, &cancel).is_err());
        assert!(decode_zlib(&input, MAX_BLOCK + 1, &cancel).is_err());
        assert!(decode_zlib(&input, raw.len(), &AtomicBool::new(true)).is_err());
        assert!(decode_with_limit(&input, raw.len(), &cancel, 1).is_err());
        assert_eq!(decode_zlib(&input, raw.len(), &cancel).unwrap().bytes, raw);
    }
    #[test]
    fn aggregate_allocator_cap_and_reentrancy_are_enforced_without_global_state() {
        let budget = BudgetScope::enter(CONTEXT_BOUND).unwrap();
        assert!(BudgetScope::enter(CONTEXT_BOUND).is_err());
        let a = unsafe { bounded_alloc(20_000) };
        let b = unsafe { bounded_alloc(20_000) };
        let c = unsafe { bounded_alloc(20_000) };
        assert!(!a.is_null() && !b.is_null() && !c.is_null());
        assert!(unsafe { bounded_alloc(20_000) }.is_null());
        assert!(unsafe { bounded_alloc(usize::MAX) }.is_null());
        for pointer in [b, a, c] {
            assert_eq!(pointer as usize % ALIGN, 0);
            unsafe {
                bounded_free(pointer);
            }
        }
        assert_eq!(budget.finish().unwrap(), 3 * (20_000 + PREFIX));
    }
    #[test]
    fn independent_threads_keep_distinct_context_budgets() {
        let barrier = Arc::new(Barrier::new(3));
        let workers = (0..2)
            .map(|_| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let budget = BudgetScope::enter(CONTEXT_BOUND).unwrap();
                    let pointer = unsafe { bounded_alloc(60_000) };
                    assert!(!pointer.is_null());
                    barrier.wait();
                    barrier.wait();
                    unsafe { bounded_free(pointer) };
                    assert_eq!(budget.finish().unwrap(), 60_000 + PREFIX);
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        assert!(!STATE.with(|s| s.get().active));
        barrier.wait();
        for worker in workers {
            worker.join().unwrap();
        }
    }
}
