//! Read-only GDAL VSI transport over bounded conditional HTTP requests.
//! No network proxy, unregistered pathname, or global GDAL curl cache is used.
use super::{RangeSource, RemoteMetrics};
use anyhow::{Result, anyhow, ensure};
use std::{
    collections::HashMap,
    ffi::{CStr, c_char, c_int, c_void},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};

const PREFIX: &str = "/vsiskarve/";
static SOURCES: OnceLock<Mutex<HashMap<String, Arc<State>>>> = OnceLock::new();
fn sources() -> &'static Mutex<HashMap<String, Arc<State>>> {
    SOURCES.get_or_init(|| Mutex::new(HashMap::new()))
}
pub(super) struct State {
    pub source: Mutex<RangeSource>,
    small_read_page_bytes: usize,
    cancel: AtomicUsize,
    error: Mutex<Option<String>>,
}
impl State {
    fn check(&self) -> Result<()> {
        let ptr = self.cancel.load(Ordering::Acquire);
        // SAFETY: OperationGuard installs a live shared AtomicBool only around
        // synchronous GDAL calls and clears it before its borrowed flag expires.
        // The adapter is not Sync and decoder multithreading is disabled.
        if ptr != 0 && unsafe { (&*(ptr as *const AtomicBool)).load(Ordering::Relaxed) } {
            anyhow::bail!("cancelled");
        }
        if self
            .error
            .lock()
            .map_err(|_| anyhow!("VSI error lock poisoned"))?
            .is_some()
        {
            anyhow::bail!("remote source previously failed");
        }
        Ok(())
    }
    fn fail(&self, error: String) {
        if let Ok(mut e) = self.error.lock() {
            if e.is_none() {
                *e = Some(error);
            }
        }
    }
}
pub(super) struct Registration {
    pub path: String,
    pub state: Arc<State>,
}
pub(super) struct OperationGuard<'a> {
    state: &'a State,
    _cancel: &'a AtomicBool,
}
impl Drop for OperationGuard<'_> {
    fn drop(&mut self) {
        self.state.cancel.store(0, Ordering::Release);
    }
}
impl Registration {
    pub fn new(source: RangeSource, small_read_page_bytes: usize) -> Result<Self> {
        install()?;
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let path = format!("{PREFIX}{}.tif", NEXT.fetch_add(1, Ordering::Relaxed));
        let state = Arc::new(State {
            source: Mutex::new(source),
            small_read_page_bytes,
            cancel: AtomicUsize::new(0),
            error: Mutex::new(None),
        });
        let mut entries = sources()
            .lock()
            .map_err(|_| anyhow!("VSI registry poisoned"))?;
        ensure!(entries.len() < 128, "remote source handle budget exceeded");
        entries.insert(path.clone(), state.clone());
        Ok(Self { path, state })
    }
    pub fn operation<'a>(&'a self, cancel: &'a AtomicBool) -> Result<OperationGuard<'a>> {
        crate::model::check_cancel(cancel)?;
        ensure!(
            self.state
                .cancel
                .compare_exchange(
                    0,
                    cancel as *const AtomicBool as usize,
                    Ordering::AcqRel,
                    Ordering::Acquire
                )
                .is_ok(),
            "concurrent GDAL operations on one source are unsupported"
        );
        Ok(OperationGuard {
            state: &self.state,
            _cancel: cancel,
        })
    }
    pub fn check_error(&self) -> Result<()> {
        if let Some(e) = self
            .state
            .error
            .lock()
            .map_err(|_| anyhow!("VSI error lock poisoned"))?
            .as_ref()
        {
            anyhow::bail!("remote source transport failed: {e}");
        }
        Ok(())
    }
    pub fn metrics(&self) -> RemoteMetrics {
        self.state
            .source
            .lock()
            .map(|s| s.metrics.clone())
            .unwrap_or_default()
    }
    pub fn verify(&self) -> Result<()> {
        self.check_error()?;
        self.state
            .source
            .lock()
            .map_err(|_| anyhow!("source lock poisoned"))?
            .verify_remote()
    }
    pub fn begin_query_budget(&self) -> Result<()> {
        self.check_error()?;
        self.state
            .source
            .lock()
            .map_err(|_| anyhow!("source lock poisoned"))?
            .begin_query_budget()
    }
    pub fn begin_verified_query(&self, renew: bool) -> Result<()> {
        self.check_error()?;
        self.state
            .source
            .lock()
            .map_err(|_| anyhow!("source lock poisoned"))?
            .begin_verified_query(renew)
    }
    pub fn end_verified_query(&self) -> Result<()> {
        self.check_error()?;
        self.state
            .source
            .lock()
            .map_err(|_| anyhow!("source lock poisoned"))?
            .end_verified_query()
    }
    pub fn registered_identity(&self) -> Option<super::RemoteIdentity> {
        self.state
            .source
            .lock()
            .ok()
            .map(|s| s.registered_identity())
    }
    pub fn range_bound(&self) -> usize {
        self.state
            .source
            .lock()
            .map(|s| s.limits.max_range_bytes as usize)
            .unwrap_or(0)
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        if let Ok(mut map) = sources().lock() {
            map.remove(&self.path);
        }
    }
}
struct Handle {
    state: Arc<State>,
    position: u64,
    eof: bool,
}
fn lookup(path: *const c_char) -> Option<Arc<State>> {
    if path.is_null() {
        return None;
    }
    // GDAL owns a NUL-terminated filename for the duration of the callback.
    let path = unsafe { CStr::from_ptr(path) }.to_str().ok()?;
    // GDAL removes the registered prefix before invoking plugin callbacks.
    sources()
        .lock()
        .ok()?
        .get(&format!("{PREFIX}{path}"))
        .cloned()
}
fn guarded<T>(fallback: T, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(fallback)
}
unsafe extern "C" fn stat(
    _: *mut c_void,
    path: *const c_char,
    out: *mut gdal_sys::VSIStatBufL,
    _: c_int,
) -> c_int {
    guarded(-1, || {
        let Some(state) = lookup(path) else {
            return -1;
        };
        if out.is_null() {
            return -1;
        }
        let Ok(source) = state.source.lock() else {
            return -1;
        };
        // GDAL supplies a writable VSIStatBufL. Only a read-only regular file exists.
        #[cfg(target_os = "linux")]
        {
            unsafe {
                let native = out.cast::<libc::stat64>();
                std::ptr::write_bytes(native, 0, 1);
                (*native).st_size = source.length as _;
                (*native).st_mode = 0o100444 as _;
            }
            0
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (out, source);
            -1
        }
    })
}
unsafe extern "C" fn open(
    _: *mut c_void,
    path: *const c_char,
    access: *const c_char,
) -> *mut c_void {
    guarded(std::ptr::null_mut(), || {
        if access.is_null() {
            return std::ptr::null_mut();
        }
        let mode = unsafe { CStr::from_ptr(access) }.to_bytes();
        if !matches!(mode, b"r" | b"rb") {
            return std::ptr::null_mut();
        }
        let Some(state) = lookup(path) else {
            return std::ptr::null_mut();
        };
        if state.check().is_err() {
            return std::ptr::null_mut();
        }
        Box::into_raw(Box::new(Handle {
            state,
            position: 0,
            eof: false,
        }))
        .cast()
    })
}
unsafe extern "C" fn tell(file: *mut c_void) -> u64 {
    guarded(u64::MAX, || {
        if file.is_null() {
            u64::MAX
        } else {
            unsafe { (*(file as *mut Handle)).position }
        }
    })
}
unsafe extern "C" fn seek(file: *mut c_void, offset: u64, whence: c_int) -> c_int {
    guarded(-1, || {
        if file.is_null() {
            return -1;
        }
        let h = unsafe { &mut *(file as *mut Handle) };
        let Ok(s) = h.state.source.lock() else {
            return -1;
        };
        let base = match whence {
            0 => 0,
            1 => h.position,
            2 => s.length,
            _ => return -1,
        };
        let next = if whence == 0 {
            Some(offset)
        } else {
            base.checked_add_signed(offset as i64)
        };
        let Some(next) = next else {
            return -1;
        };
        if next > s.length {
            return -1;
        }
        h.position = next;
        h.eof = false;
        0
    })
}
fn read_at(h: &mut Handle, offset: u64, output: &mut [u8]) -> Result<usize> {
    h.state.check()?;
    let mut source = h
        .state
        .source
        .lock()
        .map_err(|_| anyhow!("source lock poisoned"))?;
    ensure!(offset <= source.length, "VSI read offset outside source");
    let length = output.len().min((source.length - offset) as usize);
    for begin in (0..length).step_by(source.limits.max_range_bytes as usize) {
        h.state.check()?;
        let n = (length - begin).min(source.limits.max_range_bytes as usize);
        let bytes = source.read_small_page_range(
            offset + begin as u64,
            n as u64,
            h.state.small_read_page_bytes,
        )?;
        output[begin..begin + n].copy_from_slice(&bytes);
    }
    h.state.check()?;
    Ok(length)
}
unsafe extern "C" fn read(
    file: *mut c_void,
    buffer: *mut c_void,
    size: usize,
    count: usize,
) -> usize {
    guarded(0, || {
        if file.is_null() || buffer.is_null() || size == 0 {
            return 0;
        }
        let h = unsafe { &mut *(file as *mut Handle) };
        let Some(bytes) = size.checked_mul(count) else {
            h.state.fail("VSI read size overflow".into());
            return 0;
        };
        if bytes > 256 * 1024 * 1024 {
            h.state.fail("VSI decoder read exceeds256MiB".into());
            return 0;
        }
        let output = unsafe { std::slice::from_raw_parts_mut(buffer.cast::<u8>(), bytes) };
        match read_at(h, h.position, output) {
            Ok(n) => {
                h.position += n as u64;
                h.eof = n < bytes;
                n / size
            }
            Err(e) => {
                h.state.fail(format!("{e:#}"));
                0
            }
        }
    })
}
unsafe extern "C" fn multi(
    file: *mut c_void,
    count: c_int,
    buffers: *mut *mut c_void,
    offsets: *const u64,
    sizes: *const usize,
) -> c_int {
    guarded(-1, || {
        if file.is_null()
            || count < 0
            || count > 4096
            || buffers.is_null()
            || offsets.is_null()
            || sizes.is_null()
        {
            return -1;
        }
        let h = unsafe { &mut *(file as *mut Handle) };
        for i in 0..count as usize {
            // GDAL supplies already-required ranges. Prepare at most128 in the
            // existing transport/cache allowance, then copy in GDAL's order.
            if super::concurrent_transport_enabled() && i % 128 == 0 {
                let preparation = (|| -> Result<()> {
                    h.state.check()?;
                    let mut source = h
                        .state
                        .source
                        .lock()
                        .map_err(|_| anyhow!("source lock poisoned"))?;
                    let mut demands = Vec::with_capacity(128);
                    for j in i..(i + 128).min(count as usize) {
                        let (offset, size) = unsafe { (*offsets.add(j), *sizes.add(j)) };
                        if size == 0 || size as u64 > source.limits.max_range_bytes {
                            return Ok(());
                        }
                        demands.push((offset, size as u64));
                    }
                    demands.sort_unstable();
                    demands.dedup();
                    if demands
                        .windows(2)
                        .any(|w| w[0].0.checked_add(w[0].1).is_none_or(|end| end > w[1].0))
                    {
                        return Ok(());
                    }
                    let scratch = (source.limits.max_range_bytes as usize
                        + super::HTTP_SCRATCH_BYTES)
                        .saturating_sub(128 * 16);
                    let lifetime = source.cancel.clone();
                    let ptr = h.state.cancel.load(Ordering::Acquire);
                    // OperationGuard owns this shared flag until synchronous
                    // GDAL returns. Scoped transport workers join inside this
                    // callback, before that guard can release the borrow.
                    let cancel = if ptr == 0 {
                        &*lifetime
                    } else {
                        unsafe { &*(ptr as *const AtomicBool) }
                    };
                    source.prefetch_exact_ranges(&demands, scratch, cancel)?;
                    Ok(())
                })();
                if let Err(e) = preparation {
                    h.state.fail(format!("{e:#}"));
                    return -1;
                }
            }
            let (buffer, offset, size) =
                unsafe { (*buffers.add(i), *offsets.add(i), *sizes.add(i)) };
            if buffer.is_null() || size > 256 * 1024 * 1024 {
                h.state.fail("invalid VSI multiple range".into());
                return -1;
            }
            let output = unsafe { std::slice::from_raw_parts_mut(buffer.cast::<u8>(), size) };
            match read_at(h, offset, output) {
                Ok(n) if n == size => {}
                Ok(_) => {
                    h.state.fail("short VSI multiple range".into());
                    return -1;
                }
                Err(e) => {
                    h.state.fail(format!("{e:#}"));
                    return -1;
                }
            }
        }
        0
    })
}
unsafe extern "C" fn eof(file: *mut c_void) -> c_int {
    guarded(1, || {
        if file.is_null() {
            1
        } else {
            unsafe { (*(file as *mut Handle)).eof as c_int }
        }
    })
}
unsafe extern "C" fn close(file: *mut c_void) -> c_int {
    guarded(-1, || {
        if file.is_null() {
            return -1;
        }
        unsafe {
            drop(Box::from_raw(file as *mut Handle));
        }
        0
    })
}
fn install() -> Result<()> {
    static INSTALLED: OnceLock<bool> = OnceLock::new();
    let ok = *INSTALLED.get_or_init(|| unsafe {
        let callbacks = gdal_sys::VSIAllocFilesystemPluginCallbacksStruct();
        if callbacks.is_null() {
            return false;
        }
        (*callbacks).stat = Some(stat);
        (*callbacks).open = Some(open);
        (*callbacks).tell = Some(tell);
        (*callbacks).seek = Some(seek);
        (*callbacks).read = Some(read);
        (*callbacks).read_multi_range = Some(multi);
        (*callbacks).eof = Some(eof);
        (*callbacks).close = Some(close);
        (*callbacks).nBufferSize = 0;
        (*callbacks).nCacheSize = 0;
        let ok = gdal_sys::VSIInstallPluginHandler(c"/vsiskarve/".as_ptr(), callbacks) == 0;
        gdal_sys::VSIFreeFilesystemPluginCallbacksStruct(callbacks);
        ok
    });
    ensure!(ok, "cannot register GDAL source transport");
    Ok(())
}
