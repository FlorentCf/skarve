//! Native-resolution GeoTIFF access and bounded HTTP range transport.
use crate::model::{Band, Grid, Raster, check_cancel};
use anyhow::{Context, Result, anyhow, bail, ensure};
use gdal::{Dataset, DatasetOptions, GdalOpenFlags};
use reqwest::{blocking::Client, header};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[path = "source_adapter.rs"]
mod source_adapter;
#[path = "source_vsi.rs"]
mod source_vsi;
pub use source_adapter::{open_source, open_source_for_compile};

pub(crate) const HTTP_SCRATCH_BYTES: usize = 65_536;

#[derive(Clone, Debug)]
pub struct RemoteLimits {
    pub max_requests: u64,
    pub max_download_bytes: u64,
    pub max_range_bytes: u64,
    pub timeout_seconds: u64,
}
impl Default for RemoteLimits {
    fn default() -> Self {
        Self {
            max_requests: 256,
            max_download_bytes: 64 * 1024 * 1024,
            max_range_bytes: 4 * 1024 * 1024,
            timeout_seconds: 10,
        }
    }
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct RemoteMetrics {
    pub requests: u64,
    pub head_requests: u64,
    pub get_requests: u64,
    /// HTTP body bytes consumed; network stack header/read-ahead overhead is separate.
    pub received_bytes: u64,
    pub accepted_bytes: u64,
    pub failed_requests: u64,
    pub ranges: Vec<RemoteRange>,
    pub cache_hits: u64,
    /// Hits served from a previously fetched superset, without extra I/O.
    pub cache_contained_hits: u64,
    pub cache_misses: u64,
    pub cache_hit_bytes: u64,
    pub cache_evictions: u64,
    pub cache_resident_bytes: usize,
    pub cache_peak_bytes: usize,
    pub cache_capacity_bytes: usize,
}
#[derive(Clone, Debug, Serialize)]
pub struct RemoteRange {
    pub offset: u64,
    pub length: u64,
    pub accepted: bool,
}
/// Explicitly registered immutable HTTP source. No redirects or implicit downloads.
/// The caller allowlists the URL; never use an untrusted demo request URL here.
pub struct RangeSource {
    client: Client,
    url: reqwest::Url,
    pub length: u64,
    pub etag: String,
    pub source_id: String,
    pub metrics: RemoteMetrics,
    limits: RemoteLimits,
    cancel: Arc<AtomicBool>,
    cache: std::collections::VecDeque<((u64, u64), Vec<u8>)>,
    logical_reads: u64,
    invalidated: bool,
}
/// A validated registration range, consumed once by the format reader. It is
/// separate from the optional response cache and already charged to transport.
pub(crate) struct RegistrationPrefix {
    pub bytes: Vec<u8>,
    pub started: Instant,
    pub duration_ms: f64,
}
/// Encoded-only preparation. Savings are planned relative to the cache misses
/// at entry; the existing transport metrics record the actual requests/bytes.
#[derive(Debug, Default)]
pub(crate) struct RangePrefetch {
    pub(crate) admitted: bool,
    pub(crate) planning_ms: f64,
    pub(crate) fetch_ms: f64,
    pub(crate) planned_request_savings: usize,
    pub(crate) new_cache_charge_bytes: usize,
    pub(crate) protected_cache_charge_bytes: usize,
    /// Offset, length, start relative to helper entry, duration (milliseconds).
    pub(crate) events: Vec<(u64, u64, f64, f64)>,
}
const MAX_PREFETCH_DEMANDS: usize = 128;
// Three bounded vectors: planned ranges, distinct protected LRU positions,
// and returned events. The extra KiB covers their headers and scalar state.
const PREFETCH_CONTROL_BYTES: usize = MAX_PREFETCH_DEMANDS
    * (std::mem::size_of::<(u64, u64)>()
        + std::mem::size_of::<usize>()
        + std::mem::size_of::<(u64, u64, f64, f64)>())
    + 1024;
struct PrefixRequest<'a> {
    bytes: usize,
    expected_etag: Option<&'a str>,
}
fn required_header(headers: &header::HeaderMap, name: header::HeaderName) -> Result<&str> {
    headers
        .get(name)
        .context("required remote response header missing")?
        .to_str()
        .context("remote response header is not text")
}
fn strong_etag(headers: &header::HeaderMap) -> Result<String> {
    let value = required_header(headers, header::ETAG)?;
    ensure!(
        value.starts_with('"')
            && value.ends_with('"')
            && value.len() >= 2
            && !value.starts_with("W/"),
        "remote source requires a strong ETag"
    );
    Ok(value.to_owned())
}
impl RangeSource {
    pub fn register(url: &str, limits: RemoteLimits) -> Result<Self> {
        Self::register_cancellable(url, limits, Arc::new(AtomicBool::new(false)))
    }
    pub fn register_cancellable(
        url: &str,
        limits: RemoteLimits,
        cancel: Arc<AtomicBool>,
    ) -> Result<Self> {
        Self::register_configured(url, limits, cancel, None)
    }
    pub(crate) fn register_configured(
        url: &str,
        limits: RemoteLimits,
        cancel: Arc<AtomicBool>,
        configured: Option<&crate::source::HttpOptions>,
    ) -> Result<Self> {
        let operation_cancel = cancel.clone();
        Ok(Self::register_inner(url, limits, cancel, configured, None, &operation_cancel)?.0)
    }
    /// Fuse a known format's bounded first range with HTTP source registration.
    /// A declared generation is conditional from the first request onward.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn register_configured_prefix(
        url: &str,
        limits: RemoteLimits,
        cancel: Arc<AtomicBool>,
        configured: Option<&crate::source::HttpOptions>,
        prefix_bytes: usize,
        expected_etag: Option<&str>,
        operation_cancel: &AtomicBool,
    ) -> Result<(Self, RegistrationPrefix)> {
        let (source, prefix) = Self::register_inner(
            url,
            limits,
            cancel,
            configured,
            Some(PrefixRequest {
                bytes: prefix_bytes,
                expected_etag,
            }),
            operation_cancel,
        )?;
        Ok((source, prefix.context("missing registered prefix")?))
    }
    fn register_inner(
        url: &str,
        limits: RemoteLimits,
        cancel: Arc<AtomicBool>,
        configured: Option<&crate::source::HttpOptions>,
        prefix: Option<PrefixRequest<'_>>,
        operation_cancel: &AtomicBool,
    ) -> Result<(Self, Option<RegistrationPrefix>)> {
        check_cancel(operation_cancel)?;
        if let Some(prefix) = &prefix {
            ensure!(
                prefix.bytes > 0
                    && prefix.bytes <= 4 << 20
                    && prefix.bytes as u64 <= limits.max_range_bytes
                    && prefix.bytes as u64 <= limits.max_download_bytes,
                "registration prefix exceeds range or download budget"
            );
        }
        check_cancel(&cancel)?;
        ensure!(
            limits.max_requests >= 1
                && limits.max_range_bytes > 0
                && limits.max_download_bytes > 0
                && limits.timeout_seconds > 0,
            "remote budgets must be positive"
        );
        let url = reqwest::Url::parse(url).context("invalid registered source URL")?;
        ensure!(
            matches!(url.scheme(), "http" | "https"),
            "remote source must use HTTP(S)"
        );
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && (configured.is_some() || url.query().is_none())
                && url.fragment().is_none(),
            "credential-bearing or signed source URLs are unsupported"
        );
        let mut headers = header::HeaderMap::new();
        if let Some(config) = configured {
            ensure!(
                url.scheme() == "https" || config.allow_http,
                "plain HTTP requires explicit allow_http"
            );
            ensure!(
                config.headers.len() + config.header_env.len() <= 32,
                "too many HTTP authentication headers"
            );
            let mut insert = |name: &str, value: &str| -> Result<()> {
                ensure!(
                    name.len() <= 128 && value.len() <= 8192,
                    "HTTP header exceeds budget"
                );
                ensure!(
                    ![
                        "range",
                        "if-match",
                        "accept-encoding",
                        "host",
                        "content-length"
                    ]
                    .contains(&name.to_ascii_lowercase().as_str()),
                    "reserved HTTP header cannot be overridden"
                );
                let name = header::HeaderName::from_bytes(name.as_bytes())
                    .context("invalid HTTP header name")?;
                let value = header::HeaderValue::from_str(value)
                    .map_err(|_| anyhow!("invalid HTTP header value"))?;
                ensure!(!headers.contains_key(&name), "duplicate HTTP header");
                headers.insert(name, value);
                Ok(())
            };
            for (name, value) in &config.headers {
                insert(name, value)?;
            }
            for (name, var) in &config.header_env {
                ensure!(
                    var.len() <= 128 && var.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                    "invalid credential environment variable name"
                );
                let value = std::env::var(var).map_err(|_| {
                    anyhow!("configured credential environment variable is unavailable")
                })?;
                insert(name, &value)?;
            }
        }
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(limits.timeout_seconds))
            .default_headers(headers)
            .build()?;
        check_cancel(&cancel)?;
        check_cancel(operation_cancel)?;
        // A presigned S3 GET URL is method-bound and cannot generally serve HEAD.
        // Ordinary readers retain their one-byte probe; known SKV reads its
        // bootstrap directly. Neither path silently retries or downloads whole.
        let get_probe = configured.is_some() && url.query().is_some();
        let get_bytes = prefix
            .as_ref()
            .map(|p| p.bytes)
            .or_else(|| get_probe.then_some(1));
        let mut request = if let Some(bytes) = get_bytes {
            client
                .get(url.clone())
                .header(header::RANGE, format!("bytes=0-{}", bytes - 1))
        } else {
            client.head(url.clone())
        };
        if let Some(etag) = prefix.as_ref().and_then(|p| p.expected_etag) {
            request = request.header(header::IF_MATCH, etag);
        }
        let request_started = Instant::now();
        let response = request.header(header::ACCEPT_ENCODING, "identity").send();
        check_cancel(&cancel)?;
        check_cancel(operation_cancel)?;
        let response = response.map_err(|_| anyhow!("remote metadata request failed"))?;
        ensure!(
            response.status()
                == if get_bytes.is_some() {
                    reqwest::StatusCode::PARTIAL_CONTENT
                } else {
                    reqwest::StatusCode::OK
                },
            "remote metadata requires exact HEAD200 or bounded GET206"
        );
        let etag = strong_etag(response.headers())?;
        if let Some(expected) = prefix.as_ref().and_then(|p| p.expected_etag) {
            ensure!(etag == expected, "remote expected ETag mismatch");
        }
        let length = if let Some(prefix) = &prefix {
            let value = required_header(response.headers(), header::CONTENT_RANGE)?;
            let (_, total) = value
                .rsplit_once('/')
                .context("remote prefix Content-Range mismatch")?;
            let total = total
                .parse::<u64>()
                .context("invalid remote prefix total length")?;
            ensure!(
                total > 0 && total <= i64::MAX as u64,
                "invalid remote source length"
            );
            let actual = total.min(prefix.bytes as u64);
            ensure!(
                value == format!("bytes 0-{}/{}", actual - 1, total),
                "remote prefix Content-Range mismatch"
            );
            ensure!(
                required_header(response.headers(), header::CONTENT_LENGTH)?.parse::<u64>()?
                    == actual,
                "remote prefix Content-Length mismatch"
            );
            total
        } else if get_probe {
            ensure!(
                required_header(response.headers(), header::CONTENT_LENGTH)? == "1",
                "remote metadata range length mismatch"
            );
            required_header(response.headers(), header::CONTENT_RANGE)?
                .strip_prefix("bytes 0-0/")
                .context("remote metadata Content-Range mismatch")?
                .parse::<u64>()?
        } else {
            required_header(response.headers(), header::CONTENT_LENGTH)?
                .parse::<u64>()
                .context("invalid remote Content-Length")?
        };
        ensure!(
            length > 0 && length <= i64::MAX as u64,
            "invalid remote source length"
        );
        if get_bytes.is_none() {
            ensure!(
                required_header(response.headers(), header::ACCEPT_RANGES)?
                    .eq_ignore_ascii_case("bytes"),
                "remote source does not advertise byte ranges"
            );
        }
        if let Some(encoding) = response.headers().get(header::CONTENT_ENCODING) {
            ensure!(
                encoding == "identity",
                "HTTP content encoding is unsupported"
            );
        }
        let mut registered_prefix = None;
        let accepted_prefix_bytes = if let Some(prefix) = &prefix {
            let length = length.min(prefix.bytes as u64) as usize;
            let mut received = 0;
            let bytes =
                read_bounded_response(response, length, &cancel, operation_cancel, &mut received)
                    .context("remote registration prefix body incomplete")?;
            debug_assert_eq!(received, length as u64);
            registered_prefix = Some(RegistrationPrefix {
                bytes,
                started: request_started,
                duration_ms: request_started.elapsed().as_secs_f64() * 1000.,
            });
            length as u64
        } else if get_probe {
            let mut body = Vec::new();
            response
                .take(2)
                .read_to_end(&mut body)
                .map_err(|_| anyhow!("remote metadata body incomplete"))?;
            ensure!(body.len() == 1, "remote metadata range body mismatch");
            1
        } else {
            0
        };
        check_cancel(&cancel)?;
        check_cancel(operation_cancel)?;
        let mut digest = Sha256::new();
        digest.update(url.as_str().as_bytes());
        digest.update(etag.as_bytes());
        digest.update(length.to_le_bytes());
        let source_id = format!("http-etag-sha256:{:x}", digest.finalize());
        Ok((
            Self {
                client,
                url,
                length,
                etag,
                source_id,
                metrics: RemoteMetrics {
                    requests: 1,
                    head_requests: u64::from(get_bytes.is_none()),
                    get_requests: u64::from(get_bytes.is_some()),
                    received_bytes: accepted_prefix_bytes,
                    accepted_bytes: accepted_prefix_bytes,
                    ranges: if get_bytes.is_some() {
                        vec![RemoteRange {
                            offset: 0,
                            length: accepted_prefix_bytes,
                            accepted: true,
                        }]
                    } else {
                        vec![]
                    },
                    cache_capacity_bytes: configured.map_or(0, |c| c.cache_bytes),
                    ..Default::default()
                },
                limits,
                cancel,
                cache: std::collections::VecDeque::new(),
                logical_reads: 0,
                invalidated: false,
            },
            registered_prefix,
        ))
    }
    pub(crate) fn range_byte_limit(&self) -> u64 {
        self.limits.max_range_bytes
    }
    pub(crate) fn cache_byte_capacity(&self) -> usize {
        self.metrics.cache_capacity_bytes
    }
    /// Prepare whole future demands using only the existing response cache.
    /// Inputs must be sorted, nonoverlapping, and already required by the caller.
    /// No gap is fetched, no future demand is split, and no payload is decoded.
    /// An unprofitable or unadmitted plan leaves the cache and counters unchanged.
    pub(crate) fn prefetch_exact_ranges(
        &mut self,
        demands: &[(u64, u64)],
        scratch_limit: usize,
        cancel: &AtomicBool,
    ) -> Result<RangePrefetch> {
        let started = Instant::now();
        let no_op = |lifetime_cancel: &AtomicBool| -> Result<RangePrefetch> {
            check_cancel(lifetime_cancel)?;
            check_cancel(cancel)?;
            Ok(RangePrefetch {
                planning_ms: started.elapsed().as_secs_f64() * 1000.0,
                ..Default::default()
            })
        };
        ensure!(
            !self.invalidated,
            "remote source handle invalidated by an earlier failure"
        );
        check_cancel(&self.cancel)?;
        check_cancel(cancel)?;
        ensure!(
            demands.len() <= MAX_PREFETCH_DEMANDS,
            "too many prefetch demand ranges"
        );
        let mut previous_end = 0;
        for &(offset, length) in demands {
            ensure!(
                length > 0 && length <= self.limits.max_range_bytes,
                "HTTP range exceeds per-request byte budget"
            );
            let end = offset.checked_add(length).context("HTTP range overflow")?;
            ensure!(end <= self.length, "HTTP range lies outside source");
            ensure!(
                offset >= previous_end,
                "prefetch demands overlap or are unsorted"
            );
            previous_end = end;
        }
        let Some(response_scratch) =
            scratch_limit.checked_sub(PREFETCH_CONTROL_BYTES + HTTP_SCRATCH_BYTES)
        else {
            return no_op(&self.cancel);
        };
        let response_limit = (response_scratch as u64)
            .min(self.limits.max_range_bytes)
            .min(self.cache_byte_capacity().saturating_sub(128) as u64);
        if demands.len() < 2 || response_limit == 0 {
            return no_op(&self.cancel);
        }
        let mut planned: Vec<(u64, u64)> = Vec::with_capacity(demands.len());
        let mut protected: Vec<usize> = Vec::with_capacity(demands.len());
        let mut protected_charge = 0usize;
        let mut missing = 0usize;
        for &(offset, length) in demands {
            let end = offset + length; // Validated above.
            if let Some(position) = self.cache.iter().position(|((begin, size), _)| {
                offset >= *begin && end <= begin.saturating_add(*size)
            }) {
                if !protected.contains(&position) {
                    protected.push(position);
                    protected_charge = protected_charge
                        .checked_add(self.cache[position].1.len())
                        .and_then(|n| n.checked_add(128))
                        .context("prefetch cache charge overflow")?;
                }
                continue;
            }
            if length > response_limit {
                return no_op(&self.cancel);
            }
            missing += 1;
            if let Some((begin, size)) = planned.last_mut()
                && begin.checked_add(*size) == Some(offset)
                && size
                    .checked_add(length)
                    .is_some_and(|n| n <= response_limit)
            {
                *size += length;
            } else {
                planned.push((offset, length));
            }
        }
        if planned.len() >= missing {
            return no_op(&self.cancel);
        }
        let planned_bytes = planned.iter().try_fold(0u64, |sum, (_, length)| {
            sum.checked_add(*length)
                .context("prefetch byte sum overflow")
        })?;
        let new_charge = usize::try_from(planned_bytes)
            .ok()
            .and_then(|n| n.checked_add(planned.len() * 128))
            .context("prefetch cache charge overflow")?;
        // The protected entries and *all* missing demands (including isolated
        // singletons) must coexist. Unneeded entries may be evicted normally.
        // Reserve later logical demand calls too; prefetch must not consume
        // the headroom that its own subsequent cache hits require.
        if protected_charge
            .checked_add(new_charge)
            .is_none_or(|n| n > self.cache_byte_capacity())
            || protected.len() + planned.len() > 2048
            || self
                .metrics
                .requests
                .checked_add(planned.len() as u64)
                .is_none_or(|n| n > self.limits.max_requests)
            || self
                .metrics
                .received_bytes
                .checked_add(planned_bytes)
                .is_none_or(|n| n > self.limits.max_download_bytes)
            || self
                .logical_reads
                .checked_add((planned.len() + demands.len()) as u64)
                .is_none_or(|n| n > self.limits.max_requests.saturating_mul(16))
        {
            return no_op(&self.cancel);
        }
        check_cancel(&self.cancel)?;
        check_cancel(cancel)?;
        // Remove original positions in ascending order, adjusting for earlier
        // removals, and append them. Both partitions preserve their LRU order.
        // This moves existing Vec handles only; no payload copy or new cache.
        protected.sort_unstable();
        for (removed, original) in protected.iter().enumerate() {
            let entry = self
                .cache
                .remove(original - removed)
                .expect("protected cache position");
            self.cache.push_back(entry);
        }
        let mut result = RangePrefetch {
            admitted: true,
            planned_request_savings: missing - planned.len(),
            new_cache_charge_bytes: new_charge,
            protected_cache_charge_bytes: protected_charge,
            events: Vec::with_capacity(planned.len()),
            ..Default::default()
        };
        result.planning_ms = started.elapsed().as_secs_f64() * 1000.0;
        let fetch_started = Instant::now();
        for (offset, length) in planned {
            let event_started = Instant::now();
            // At most one returned response is live here. Its cloned cache
            // entry is charged above; HTTP_SCRATCH_BYTES and this response are
            // charged to scratch_limit, separately from retained cache bytes.
            drop(self.read_range_cancellable(offset, length, cancel)?);
            result.events.push((
                offset,
                length,
                event_started.duration_since(started).as_secs_f64() * 1000.0,
                event_started.elapsed().as_secs_f64() * 1000.0,
            ));
        }
        result.fetch_ms = fetch_started.elapsed().as_secs_f64() * 1000.0;
        Ok(result)
    }
    pub fn read_range(&mut self, offset: u64, length: u64) -> Result<Vec<u8>> {
        let cancel = self.cancel.clone();
        self.read_range_cancellable(offset, length, &cancel)
    }
    /// Borrowed per-operation cancellation supplements the lifetime flag. The
    /// borrow cannot outlive this synchronous read; body reads check both flags.
    pub(crate) fn read_range_cancellable(
        &mut self,
        offset: u64,
        length: u64,
        cancel: &AtomicBool,
    ) -> Result<Vec<u8>> {
        ensure!(
            !self.invalidated,
            "remote source handle invalidated by an earlier failure"
        );
        check_cancel(&self.cancel)?;
        check_cancel(cancel)?;
        self.logical_reads = self.logical_reads.saturating_add(1);
        ensure!(
            self.logical_reads <= self.limits.max_requests.saturating_mul(16),
            "source logical range work budget exhausted"
        );
        ensure!(
            length > 0 && length <= self.limits.max_range_bytes,
            "HTTP range exceeds per-request byte budget"
        );
        let end = offset.checked_add(length).context("HTTP range overflow")?;
        ensure!(end <= self.length, "HTTP range lies outside source");
        if let Some(position) = self
            .cache
            .iter()
            .position(|((begin, size), _)| offset >= *begin && end <= begin.saturating_add(*size))
        {
            let entry = self.cache.remove(position).expect("cache position exists");
            let begin = (offset - entry.0.0) as usize;
            let result = entry.1[begin..begin + length as usize].to_vec();
            self.metrics.cache_contained_hits += u64::from(entry.0 != (offset, length));
            self.cache.push_back(entry);
            self.metrics.cache_hits += 1;
            self.metrics.cache_hit_bytes += length;
            check_cancel(&self.cancel)?;
            check_cancel(cancel)?;
            return Ok(result);
        }
        self.metrics.cache_misses += 1;
        let data = self.read_uncached_range(offset, length, cancel)?;
        let charged = data.len().saturating_add(128);
        if charged <= self.metrics.cache_capacity_bytes {
            while self.metrics.cache_resident_bytes.saturating_add(charged)
                > self.metrics.cache_capacity_bytes
                || self.cache.len() >= 2048
            {
                if let Some((_, old)) = self.cache.pop_front() {
                    self.metrics.cache_resident_bytes -= old.len() + 128;
                    self.metrics.cache_evictions += 1;
                } else {
                    break;
                }
            }
            self.cache.push_back(((offset, length), data.clone()));
            self.metrics.cache_resident_bytes += charged;
            self.metrics.cache_peak_bytes = self
                .metrics
                .cache_peak_bytes
                .max(self.metrics.cache_resident_bytes);
        }
        check_cancel(&self.cancel)?;
        check_cancel(cancel)?;
        Ok(data)
    }
    fn read_uncached_range(
        &mut self,
        offset: u64,
        length: u64,
        cancel: &AtomicBool,
    ) -> Result<Vec<u8>> {
        check_cancel(&self.cancel)?;
        check_cancel(cancel)?;
        ensure!(
            length > 0 && length <= self.limits.max_range_bytes,
            "HTTP range exceeds per-request byte budget"
        );
        let end = offset.checked_add(length).context("HTTP range overflow")?;
        ensure!(end <= self.length, "HTTP range lies outside source");
        ensure!(
            self.metrics.requests < self.limits.max_requests,
            "HTTP request budget exhausted"
        );
        ensure!(
            self.metrics
                .received_bytes
                .checked_add(length)
                .is_some_and(|n| n <= self.limits.max_download_bytes),
            "HTTP download budget exhausted"
        );
        self.metrics.requests += 1;
        self.metrics.get_requests += 1;
        let result = self.read_range_inner(offset, length, cancel);
        self.metrics.ranges.push(RemoteRange {
            offset,
            length,
            accepted: result.is_ok(),
        });
        if result.is_err() {
            self.metrics.failed_requests += 1;
            self.invalidated = true;
            self.cache.clear();
            self.metrics.cache_resident_bytes = 0;
        }
        result
    }
    pub(crate) fn verify_remote(&mut self) -> Result<()> {
        ensure!(
            !self.invalidated,
            "remote source handle invalidated by an earlier failure"
        );
        // Revalidation must contact the provider even when the byte is cached.
        if self.url.query().is_some() {
            let cancel = self.cancel.clone();
            self.read_uncached_range(0, 1, &cancel)?;
            return Ok(());
        }
        check_cancel(&self.cancel)?;
        ensure!(
            self.metrics.requests < self.limits.max_requests,
            "HTTP request budget exhausted"
        );
        self.metrics.requests += 1;
        self.metrics.head_requests += 1;
        let result = (|| {
            let response = self
                .client
                .head(self.url.clone())
                .header(header::IF_MATCH, &self.etag)
                .header(header::ACCEPT_ENCODING, "identity")
                .send()
                .map_err(|_| anyhow!("remote verification failed"))?;
            ensure!(
                response.status() == reqwest::StatusCode::OK,
                "remote source changed or verification failed"
            );
            ensure!(
                strong_etag(response.headers())? == self.etag
                    && required_header(response.headers(), header::CONTENT_LENGTH)?
                        .parse::<u64>()?
                        == self.length,
                "remote source identity changed"
            );
            check_cancel(&self.cancel)
        })();
        if result.is_err() {
            self.metrics.failed_requests += 1;
            self.invalidated = true;
            self.cache.clear();
            self.metrics.cache_resident_bytes = 0;
        }
        result
    }
    fn read_range_inner(
        &mut self,
        offset: u64,
        length: u64,
        cancel: &AtomicBool,
    ) -> Result<Vec<u8>> {
        check_cancel(&self.cancel)?;
        check_cancel(cancel)?;
        let end = offset + length - 1;
        let response = self
            .client
            .get(self.url.clone())
            .header(header::RANGE, format!("bytes={offset}-{end}"))
            .header(header::IF_MATCH, &self.etag)
            .header(header::ACCEPT_ENCODING, "identity")
            .send();
        check_cancel(&self.cancel)?;
        check_cancel(cancel)?;
        let response = response.map_err(|_| anyhow!("remote range request failed"))?;
        // Check status before consuming any body: never fall back to a whole download.
        ensure!(
            response.status() == reqwest::StatusCode::PARTIAL_CONTENT,
            "remote range requires 206; source changed, request failed, or server ignored Range"
        );
        ensure!(
            strong_etag(response.headers())? == self.etag,
            "remote source changed ETag"
        );
        ensure!(
            required_header(response.headers(), header::CONTENT_RANGE)?
                == format!("bytes {offset}-{end}/{}", self.length),
            "remote Content-Range does not match request"
        );
        ensure!(
            required_header(response.headers(), header::CONTENT_LENGTH)?.parse::<u64>()? == length,
            "remote response length does not match request"
        );
        if let Some(encoding) = response.headers().get(header::CONTENT_ENCODING) {
            ensure!(
                encoding == "identity",
                "HTTP content encoding is unsupported"
            );
        }
        let body = read_bounded_response(
            response,
            usize::try_from(length).context("HTTP range too large for host")?,
            &self.cancel,
            cancel,
            &mut self.metrics.received_bytes,
        )?;
        self.metrics.accepted_bytes += length;
        Ok(body)
    }
}

fn read_bounded_response(
    response: reqwest::blocking::Response,
    length: usize,
    lifetime_cancel: &AtomicBool,
    operation_cancel: &AtomicBool,
    received_bytes: &mut u64,
) -> Result<Vec<u8>> {
    check_cancel(lifetime_cancel)?;
    check_cancel(operation_cancel)?;
    let mut body = Vec::with_capacity(length);
    let mut reader = response.take(length as u64 + 1);
    let mut chunk = vec![0u8; HTTP_SCRATCH_BYTES];
    loop {
        check_cancel(lifetime_cancel)?;
        check_cancel(operation_cancel)?;
        let read_result = reader.read(&mut chunk);
        if let Ok(n) = read_result {
            *received_bytes += n as u64;
        }
        check_cancel(lifetime_cancel)?;
        check_cancel(operation_cancel)?;
        let n = read_result.context("remote body incomplete")?;
        if n == 0 {
            break;
        }
        ensure!(
            body.len().saturating_add(n) <= length,
            "remote body exceeds requested length"
        );
        body.extend_from_slice(&chunk[..n]);
    }
    check_cancel(lifetime_cancel)?;
    check_cancel(operation_cancel)?;
    ensure!(body.len() == length, "remote body length mismatch");
    Ok(body)
}

struct Gateway {
    address: String,
    path: String,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    source: Arc<Mutex<RangeSource>>,
    error: Arc<Mutex<Option<String>>>,
}
impl Gateway {
    fn start(source: RangeSource) -> Result<Self> {
        // GDAL's /vsicurl cache is process-global. An ephemeral TCP port can be
        // reused, so every gateway needs a distinct URL even for identical ETags.
        static NEXT_GATEWAY: AtomicU64 = AtomicU64::new(1);
        let path = format!(
            "/source-{}.tif",
            NEXT_GATEWAY.fetch_add(1, Ordering::Relaxed)
        );
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?.to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let source = Arc::new(Mutex::new(source));
        let error = Arc::new(Mutex::new(None));
        let thread_stop = stop.clone();
        let thread_source = source.clone();
        let thread_error = error.clone();
        let thread_path = path.clone();
        let worker = thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut connection, _)) => {
                        if let Err(err) =
                            serve_connection(&mut connection, &thread_source, &thread_path)
                        {
                            *thread_error.lock().unwrap() = Some(format!("{err:#}"));
                            let _ = connection.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1))
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            address,
            path,
            stop,
            worker: Some(worker),
            source,
            error,
        })
    }
}
impl Drop for Gateway {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn serve_connection(
    connection: &mut TcpStream,
    shared: &Arc<Mutex<RangeSource>>,
    path: &str,
) -> Result<()> {
    connection.set_read_timeout(Some(Duration::from_secs(3)))?;
    connection.set_write_timeout(Some(Duration::from_secs(3)))?;
    let mut bytes = Vec::new();
    loop {
        ensure!(bytes.len() < 8192, "gateway request headers too large");
        let mut byte = [0u8; 1];
        connection.read_exact(&mut byte)?;
        bytes.push(byte[0]);
        if bytes.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let text = std::str::from_utf8(&bytes)?;
    let mut lines = text.split("\r\n");
    let request: Vec<_> = lines
        .next()
        .context("gateway missing request")?
        .split_whitespace()
        .collect();
    ensure!(
        request.len() == 3 && request[1] == path,
        "gateway path is not registered"
    );
    let mut range = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("range") {
                ensure!(range.is_none(), "duplicate Range header");
                range = Some(value.trim());
            }
        }
    }
    let mut source = shared.lock().map_err(|_| anyhow!("source lock poisoned"))?;
    check_cancel(&source.cancel)?;
    if request[0] == "HEAD" {
        write!(
            connection,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nETag: {}\r\nConnection: close\r\n\r\n",
            source.length, source.etag
        )?;
        return Ok(());
    }
    ensure!(request[0] == "GET", "gateway method unsupported");
    let range = range
        .context("gateway refuses non-range GET")?
        .strip_prefix("bytes=")
        .context("gateway requires byte Range")?;
    let (start, end) = range.split_once('-').context("gateway range malformed")?;
    let start = start.parse::<u64>()?;
    let end = end.parse::<u64>()?.min(source.length - 1);
    ensure!(end >= start, "gateway range malformed");
    let body = source.read_range(start, end - start + 1)?;
    write!(
        connection,
        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nAccept-Ranges: bytes\r\nETag: {}\r\nConnection: close\r\n\r\n",
        body.len(),
        start,
        end,
        source.length,
        source.etag
    )?;
    connection.write_all(&body)?;
    Ok(())
}
struct GdalOptions(Vec<(&'static str, String)>);
impl GdalOptions {
    fn remote() -> Result<Self> {
        let mut prior = Vec::new();
        for (key, value) in [
            ("GDAL_DISABLE_READDIR_ON_OPEN", "EMPTY_DIR"),
            ("CPL_VSIL_CURL_ALLOWED_EXTENSIONS", ".tif"),
            ("GDAL_HTTP_TIMEOUT", "15"),
            ("GDAL_HTTP_MAX_RETRY", "0"),
            ("VSI_CACHE", "FALSE"),
            ("GDAL_NUM_THREADS", "1"),
        ] {
            prior.push((key, gdal::config::get_thread_local_config_option(key, "")?));
            gdal::config::set_thread_local_config_option(key, value)?;
        }
        Ok(Self(prior))
    }
}
impl Drop for GdalOptions {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            if value.is_empty() {
                let _ = gdal::config::clear_thread_local_config_option(key);
            } else {
                let _ = gdal::config::set_thread_local_config_option(key, value);
            }
        }
    }
}
/// Selective remote COG/GeoTIFF decoding. Masks must be internal; no sidecar discovery.
pub fn open_remote_window(
    url: &str,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    max_bytes: usize,
    limits: RemoteLimits,
) -> Result<(Raster, RemoteMetrics)> {
    open_remote_window_cancellable(
        url,
        x,
        y,
        width,
        height,
        max_bytes,
        limits,
        Arc::new(AtomicBool::new(false)),
    )
}
/// Cooperative cancellation around native/HTTP calls and between 64 KiB body chunks.
/// An in-flight blocking HTTP read remains bounded by the configured request timeout.
pub fn open_remote_window_cancellable(
    url: &str,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    max_bytes: usize,
    limits: RemoteLimits,
    cancel: Arc<AtomicBool>,
) -> Result<(Raster, RemoteMetrics)> {
    check_cancel(&cancel)?;
    let source = RangeSource::register_cancellable(url, limits, cancel.clone())?;
    let source_id = source.source_id.clone();
    let gateway = Gateway::start(source)?;
    let _options = GdalOptions::remote()?;
    let path = format!("/vsicurl/http://{}{}", gateway.address, gateway.path);
    let result = (|| {
        let dataset = open_gtiff(&path)?;
        check_cancel(&cancel)?;
        let meta = metadata(&dataset, source_id)?;
        read_window(&dataset, &meta, x, y, width, height, max_bytes, &cancel)
    })();
    check_cancel(&cancel)?;
    if let Some(error) = gateway
        .error
        .lock()
        .map_err(|_| anyhow!("gateway error lock poisoned"))?
        .clone()
    {
        bail!("remote range gateway rejected source: {error}");
    }
    let raster = result?;
    let metrics = gateway
        .source
        .lock()
        .map_err(|_| anyhow!("source lock poisoned"))?
        .metrics
        .clone();
    Ok((raster, metrics))
}

pub use crate::source::{BandMetadata, RasterMetadata, ReadMetrics, WindowSource};
/// Stable open dataset. The caller must keep source and sidecars immutable.
pub struct LocalSource {
    dataset: Dataset,
    path: PathBuf,
    signature: String,
    pub metadata: RasterMetadata,
}
fn fingerprint(path: &Path) -> Result<String> {
    let stat = fs::metadata(path).context("cannot stat raster source")?;
    ensure!(stat.is_file(), "raster source must be a regular file");
    let mut digest = Sha256::new();
    digest.update(path.as_os_str().as_encoded_bytes());
    digest.update(stat.len().to_le_bytes());
    digest.update(format!("{:?}", stat.modified()?).as_bytes());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        digest.update(stat.dev().to_le_bytes());
        digest.update(stat.ino().to_le_bytes());
        digest.update(stat.ctime().to_le_bytes());
        digest.update(stat.ctime_nsec().to_le_bytes());
    }
    for suffix in [".msk", ".aux.xml", ".ovr", ".msk.ovr", ".aux"] {
        let sibling = PathBuf::from(format!("{}{suffix}", path.to_string_lossy()));
        if sibling.exists() {
            let meta = fs::metadata(sibling)?;
            digest.update(suffix.as_bytes());
            digest.update(meta.len().to_le_bytes());
            digest.update(format!("{:?}", meta.modified()?).as_bytes());
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                digest.update(meta.dev().to_le_bytes());
                digest.update(meta.ino().to_le_bytes());
                digest.update(meta.ctime().to_le_bytes());
                digest.update(meta.ctime_nsec().to_le_bytes());
            }
        }
    }
    Ok(format!("local-stat-sha256:{:x}", digest.finalize()))
}
/// Pin file metadata at lazy registration without opening GDAL or raster data.
pub(crate) fn registration_signature(path: &str) -> Result<String> {
    fingerprint(&fs::canonicalize(path).context("local raster path does not exist")?)
}
fn open_gtiff(path: &str) -> Result<Dataset> {
    Dataset::open_ex(
        path,
        DatasetOptions {
            open_flags: GdalOpenFlags::GDAL_OF_RASTER | GdalOpenFlags::GDAL_OF_READONLY,
            allowed_drivers: Some(&["GTiff"]),
            ..Default::default()
        },
    )
    .context("cannot open supported GeoTIFF source")
}
fn metadata(dataset: &Dataset, source_id: String) -> Result<RasterMetadata> {
    let indices: Vec<_> = (0..dataset.raster_count()).collect();
    metadata_selected(dataset, source_id, &indices)
}
fn metadata_selected(
    dataset: &Dataset,
    source_id: String,
    indices: &[usize],
) -> Result<RasterMetadata> {
    metadata_selected_with_crs(dataset, source_id, indices, None)
}
fn metadata_selected_with_crs(
    dataset: &Dataset,
    source_id: String,
    indices: &[usize],
    crs_override: Option<&str>,
) -> Result<RasterMetadata> {
    metadata_selected_with_crs_limit(dataset, source_id, indices, crs_override, 20)
}
fn metadata_selected_with_crs_limit(
    dataset: &Dataset,
    source_id: String,
    indices: &[usize],
    crs_override: Option<&str>,
    max_bands: usize,
) -> Result<RasterMetadata> {
    let (width, height) = dataset.raster_size();
    ensure!(width > 0 && height > 0, "raster dimensions must be nonzero");
    let transform = dataset
        .geo_transform()
        .context("raster needs an affine geotransform")?;
    ensure!(
        transform.iter().all(|v| v.is_finite()),
        "nonfinite raster transform"
    );
    ensure!(
        transform[2] == 0.0 && transform[4] == 0.0,
        "rotated/skewed raster grids are unsupported"
    );
    ensure!(
        transform[1] > 0.0 && transform[5] < 0.0,
        "only north-up grids with dx > 0 and dy < 0 are supported"
    );
    let spatial_ref = if let Some(definition) = crs_override {
        ensure!(
            !definition.is_empty() && definition.len() <= 4096,
            "explicit source CRS exceeds budget"
        );
        let mut assigned = gdal::spatial_ref::SpatialRef::from_definition(definition)
            .context("invalid explicit source CRS")?;
        assigned
            .set_axis_mapping_strategy(gdal::spatial_ref::AxisMappingStrategy::TraditionalGisOrder);
        if let Ok(mut existing) = dataset.spatial_ref() {
            // Native affine coordinates already use x/y; compare references
            // under that same explicit axis convention, without transforming.
            existing.set_axis_mapping_strategy(
                gdal::spatial_ref::AxisMappingStrategy::TraditionalGisOrder,
            );
            ensure!(
                existing == assigned,
                "explicit source CRS conflicts with embedded CRS; reprojection is unsupported"
            );
        }
        assigned
    } else {
        dataset
            .spatial_ref()
            .context("raster CRS is missing or unknown")?
    };
    ensure!(
        spatial_ref.is_geographic() || spatial_ref.is_projected(),
        "raster CRS must be an explicit geographic/projected CRS"
    );
    let crs = spatial_ref
        .authority()
        .or_else(|_| spatial_ref.to_wkt())
        .context("cannot identify raster CRS")?;
    ensure!(!crs.trim().is_empty(), "raster CRS is missing");
    let count = indices.len();
    ensure!(
        (1..=max_bands).contains(&count) && max_bands <= 64,
        "raster must have 1..{max_bands} aligned bands"
    );
    let mut bands = Vec::with_capacity(count);
    ensure!(
        indices.iter().all(|&i| i < dataset.raster_count()),
        "source band index out of range"
    );
    for &i in indices {
        let band = dataset.rasterband(i + 1)?;
        let data_type = band.band_type().name();
        ensure!(
            matches!(
                data_type.as_str(),
                "Byte" | "Int8" | "UInt16" | "Int16" | "UInt32" | "Int32" | "Float32" | "Float64"
            ),
            "unsupported raster data type {data_type}; Int64/UInt64 and complex values are rejected"
        );
        let scale = band.scale().unwrap_or(1.0);
        let offset = band.offset().unwrap_or(0.0);
        ensure!(
            scale.is_finite() && offset.is_finite(),
            "nonfinite scale or offset"
        );
        let unit = band.unit();
        ensure!(unit.len() <= 1024, "band unit exceeds 1024 bytes");
        bands.push(BandMetadata {
            data_type,
            nodata: band.no_data_value(),
            scale,
            offset,
            unit: (!unit.is_empty()).then_some(unit),
            block_size: band.block_size(),
        });
    }
    let grid = Grid {
        width,
        height,
        transform,
        crs,
    };
    grid.validate()?;
    ensure!(
        !spatial_ref.is_geographic() || grid.crs == "EPSG:4326",
        "only EPSG:4326 is supported for geographic raster coordinates"
    );
    Ok(RasterMetadata {
        grid,
        bands,
        source_id,
    })
}
impl LocalSource {
    /// Check even on an all-cache-hit request; immutable source changes fail closed.
    pub fn verify_immutable(&self) -> Result<()> {
        ensure!(
            fingerprint(&self.path)? == self.signature,
            "source changed since opening"
        );
        Ok(())
    }
    /// Read just the requested bands, preserving their requested order.
    pub fn read_selected_window_cancellable(
        &self,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
        indices: &[usize],
        max_bytes: usize,
        cancel: &AtomicBool,
    ) -> Result<(Raster, ReadMetrics)> {
        self.verify_immutable()?;
        let result = read_selected_window(
            &self.dataset,
            &self.metadata,
            x,
            y,
            width,
            height,
            indices,
            max_bytes,
            cancel,
        )?;
        self.verify_immutable()?;
        Ok(result)
    }
    pub fn open(path: &str) -> Result<Self> {
        ensure!(
            !path.starts_with("/vsi") && !path.contains("://"),
            "local adapter accepts regular local paths only"
        );
        let path = fs::canonicalize(path).context("local raster path does not exist")?;
        let signature = fingerprint(&path)?;
        let dataset = open_gtiff(path.to_str().ok_or_else(|| anyhow!("path must be UTF-8"))?)?;
        let metadata = metadata(&dataset, signature.clone())?;
        ensure!(
            fingerprint(&path)? == signature,
            "source changed while opening"
        );
        Ok(Self {
            dataset,
            path,
            signature,
            metadata,
        })
    }
    pub fn read_window(
        &self,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
        max_bytes: usize,
    ) -> Result<Raster> {
        self.read_window_cancellable(x, y, width, height, max_bytes, &AtomicBool::new(false))
    }
    pub fn read_window_cancellable(
        &self,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
        max_bytes: usize,
        cancel: &AtomicBool,
    ) -> Result<Raster> {
        check_cancel(cancel)?;
        ensure!(
            fingerprint(&self.path)? == self.signature,
            "source changed since opening"
        );
        let raster = read_window(
            &self.dataset,
            &self.metadata,
            x,
            y,
            width,
            height,
            max_bytes,
            cancel,
        )?;
        check_cancel(cancel)?;
        ensure!(
            fingerprint(&self.path)? == self.signature,
            "source changed during window read"
        );
        Ok(raster)
    }
}
pub fn inspect_local(path: &str) -> Result<RasterMetadata> {
    Ok(LocalSource::open(path)?.metadata)
}
pub fn open_local(path: &str, max_bytes: usize) -> Result<Raster> {
    open_local_cancellable(path, max_bytes, &AtomicBool::new(false))
}
pub fn open_local_cancellable(path: &str, max_bytes: usize, cancel: &AtomicBool) -> Result<Raster> {
    check_cancel(cancel)?;
    let source = LocalSource::open(path)?;
    source.read_window_cancellable(
        0,
        0,
        source.metadata.grid.width,
        source.metadata.grid.height,
        max_bytes,
        cancel,
    )
}
pub fn open_window(
    path: &str,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    max_bytes: usize,
) -> Result<Raster> {
    LocalSource::open(path)?.read_window(x, y, width, height, max_bytes)
}
fn read_window(
    dataset: &Dataset,
    meta: &RasterMetadata,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    max_bytes: usize,
    cancel: &AtomicBool,
) -> Result<Raster> {
    let indices: Vec<_> = (0..meta.bands.len()).collect();
    Ok(read_selected_window(
        dataset, meta, x, y, width, height, &indices, max_bytes, cancel,
    )?
    .0)
}

fn selected_window_buffer_bound(
    meta: &RasterMetadata,
    width: usize,
    height: usize,
    indices: &[usize],
) -> Result<usize> {
    ensure!(
        width > 0 && height > 0,
        "window dimensions must be positive"
    );
    let n = width.checked_mul(height).context("window size overflow")?;
    let chunk_rows = (65_536 / width).max(1).min(height);
    let mask_bytes = width
        .checked_mul(chunk_rows)
        .context("mask size overflow")?;
    let output_bytes = n
        .checked_mul(9)
        .and_then(|v| v.checked_mul(indices.len()))
        .context("raster allocation overflow")?;
    let block_bytes = meta
        .bands
        .iter()
        .enumerate()
        .filter(|(i, _)| indices.contains(i))
        .map(|(_, b)| {
            b.block_size
                .0
                .checked_mul(b.block_size.1)
                .and_then(|v| v.checked_mul(8))
                .and_then(|v| v.checked_mul(indices.len()))
                .unwrap_or(usize::MAX)
        })
        .max()
        .unwrap_or(0);
    let required = output_bytes
        .checked_add(mask_bytes)
        .and_then(|v| v.checked_add(block_bytes))
        .and_then(|v| v.checked_add(32 * 1024)) // bounded CRS/units/source identity and band headers
        .context("raster allocation overflow")?;
    Ok(required)
}
fn read_selected_window(
    dataset: &Dataset,
    meta: &RasterMetadata,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    indices: &[usize],
    max_bytes: usize,
    cancel: &AtomicBool,
) -> Result<(Raster, ReadMetrics)> {
    read_selected_window_mapped(
        dataset, meta, x, y, width, height, indices, max_bytes, cancel, None,
    )
}
#[allow(clippy::too_many_arguments)]
fn read_selected_window_mapped(
    dataset: &Dataset,
    meta: &RasterMetadata,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    indices: &[usize],
    max_bytes: usize,
    cancel: &AtomicBool,
    mapping: Option<&[usize]>,
) -> Result<(Raster, ReadMetrics)> {
    check_cancel(cancel)?;
    ensure!(
        !indices.is_empty() && indices.iter().all(|&i| i < meta.bands.len()),
        "band index out of range"
    );
    let mut metrics = ReadMetrics::default();
    ensure!(
        width > 0 && height > 0,
        "window dimensions must be positive"
    );
    ensure!(
        x.checked_add(width).is_some_and(|v| v <= meta.grid.width)
            && y.checked_add(height).is_some_and(|v| v <= meta.grid.height),
        "window lies outside raster"
    );
    let n = width.checked_mul(height).context("window size overflow")?;
    let chunk_rows = (65_536 / width).max(1).min(height);
    let required = selected_window_buffer_bound(meta, width, height, indices)?;
    ensure!(
        required <= max_bytes,
        "raster window exceeds memory budget: needs at least {required} bytes, budget {max_bytes}"
    );
    let mut grid = meta.grid.clone();
    grid.width = width;
    grid.height = height;
    grid.transform[0] += x as f64 * grid.transform[1];
    grid.transform[3] += y as f64 * grid.transform[5];
    let mut bands: Vec<Band> = indices
        .iter()
        .map(|&i| Band {
            values: vec![0.; n],
            valid: vec![false; n],
            unit: meta.bands[i].unit.clone(),
        })
        .collect();
    let source_bands = indices
        .iter()
        .map(|&i| dataset.rasterband(mapping.map_or(i, |m| m[i]) + 1))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mask_bands = source_bands
        .iter()
        .map(|band| band.open_mask_band())
        .collect::<std::result::Result<Vec<_>, _>>()?;
    // The guard also releases decoded blocks on a cancelled or failed chunk.
    struct ChunkCacheGuard<'a>(Option<&'a Dataset>);
    impl Drop for ChunkCacheGuard<'_> {
        fn drop(&mut self) {
            if let Some(dataset) = self.0 {
                unsafe {
                    gdal_sys::GDALFlushCache(dataset.c_dataset());
                }
            }
        }
    }
    let _cache_guard = ChunkCacheGuard(mapping.map(|_| dataset));
    let mut read_chunk = |selected: usize, row: usize| -> Result<()> {
        check_cancel(cancel)?;
        let info = &meta.bands[indices[selected]];
        let band = &source_bands[selected];
        let mask_band = &mask_bands[selected];
        let output = &mut bands[selected];
        let rows = chunk_rows.min(height - row);
        let (begin, end) = (row * width, (row + rows) * width);
        let io_started = std::time::Instant::now();
        band.read_into_slice::<f64>(
            (x as isize, (y + row) as isize),
            (width, rows),
            (width, rows),
            &mut output.values[begin..end],
            None,
        )?;
        check_cancel(cancel)?;
        let mask = mask_band.read_as::<u8>(
            (x as isize, (y + row) as isize),
            (width, rows),
            (width, rows),
            None,
        )?;
        metrics.read_decode_ms += io_started.elapsed().as_secs_f64() * 1000.;
        metrics.raster_io_calls += 2;
        let normalization_started = std::time::Instant::now();
        for (j, (value, mask)) in output.values[begin..end]
            .iter_mut()
            .zip(mask.data())
            .enumerate()
        {
            if j % 4096 == 0 {
                check_cancel(cancel)?;
            }
            (*value, output.valid[begin + j]) = crate::source::normalize_raw_sample(
                *value,
                *mask,
                info.nodata,
                info.scale,
                info.offset,
            );
        }
        metrics.normalization_ms += normalization_started.elapsed().as_secs_f64() * 1000.;
        Ok(())
    };
    if mapping.is_some() {
        // Retain only an identical physical block footprint between chunks.
        // The X extent is constant within this window; include each data AND
        // mask band's block shape so strips/independent mask tiling cannot
        // accumulate outside the one-chunk decoder reservation. Flush before
        // any footprint transition; the guard also flushes on every exit.
        let shapes = source_bands
            .iter()
            .chain(mask_bands.iter())
            .map(|b| b.block_size())
            .collect::<Vec<_>>();
        ensure!(
            shapes.iter().all(|&(w, h)| w > 0 && h > 0),
            "invalid decoder block dimensions"
        );
        let mut prior = Vec::new();
        let mut flushes = 1; // Exit guard always releases the final footprint.
        let mut reused_chunks = 0;
        for row in (0..height).step_by(chunk_rows) {
            let rows = chunk_rows.min(height - row);
            let footprint = shapes
                .iter()
                .map(|&(bw, bh)| {
                    (
                        x / bw,
                        (x + width - 1) / bw,
                        (y + row) / bh,
                        (y + row + rows - 1) / bh,
                    )
                })
                .collect::<Vec<_>>();
            if !prior.is_empty() && prior != footprint {
                unsafe {
                    gdal_sys::GDALFlushCache(dataset.c_dataset());
                }
                flushes += 1;
            } else if !prior.is_empty() {
                reused_chunks += 1;
            }
            for selected in 0..indices.len() {
                read_chunk(selected, row)?;
            }
            prior = footprint;
        }
        metrics.decoder_cache_flushes = flushes;
        metrics.decoder_cache_reused_chunks = reused_chunks;
    } else {
        // Preserve the legacy band's outer traversal and cache policy.
        for selected in 0..indices.len() {
            for row in (0..height).step_by(chunk_rows) {
                read_chunk(selected, row)?;
            }
        }
    }
    Ok((
        Raster {
            grid,
            bands,
            source_id: meta.source_id.clone(),
        },
        metrics,
    ))
}

impl WindowSource for LocalSource {
    fn read_buffer_bound(&self, width: usize, height: usize, indices: &[usize]) -> Result<usize> {
        selected_window_buffer_bound(&self.metadata, width, height, indices)
    }
    fn metadata(&self) -> &RasterMetadata {
        &self.metadata
    }
    fn verify_immutable(&self) -> Result<()> {
        LocalSource::verify_immutable(self)
    }
    fn read_selected_window_cancellable(
        &self,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
        indices: &[usize],
        max_bytes: usize,
        cancel: &AtomicBool,
    ) -> Result<(Raster, ReadMetrics)> {
        LocalSource::read_selected_window_cancellable(
            self, x, y, width, height, indices, max_bytes, cancel,
        )
    }
}

#[cfg(test)]
mod range_operation_tests {
    use super::*;
    #[test]
    fn borrowed_cancellation_drains_body_and_quarantines_failed_reader() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/raw", listener.local_addr().unwrap());
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            for connection in listener.incoming().take(2) {
                let mut stream = connection.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                if request.starts_with(b"HEAD ") {
                    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length:131072\r\nETag:\"fixed\"\r\nAccept-Ranges:bytes\r\nConnection:close\r\n\r\n").unwrap();
                } else {
                    assert!(
                        String::from_utf8_lossy(&request)
                            .to_ascii_lowercase()
                            .contains("if-match: \"fixed\"")
                    );
                    stream.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Length:131072\r\nContent-Range:bytes 0-131071/131072\r\nETag:\"fixed\"\r\nConnection:close\r\n\r\n").unwrap();
                    stream.write_all(&[1; 65536]).unwrap();
                    stream.flush().unwrap();
                    started_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    // A promptly cancelled client may already have closed.
                    let _ = stream.write_all(&[2; 65536]);
                }
            }
        });
        let mut source = RangeSource::register(&url, RemoteLimits::default()).unwrap();
        let operation = Arc::new(AtomicBool::new(false));
        let trigger = operation.clone();
        let controller = thread::spawn(move || {
            started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            trigger.store(true, Ordering::Relaxed);
            release_tx.send(()).unwrap();
        });
        let error = source
            .read_range_cancellable(0, 131072, &operation)
            .unwrap_err();
        assert_eq!(error.to_string(), "cancelled");
        controller.join().unwrap();
        server.join().unwrap();
        assert_eq!(source.metrics.failed_requests, 1);
        assert_eq!(source.metrics.accepted_bytes, 0);
        assert!(!source.metrics.ranges[0].accepted);
        operation.store(false, Ordering::Relaxed);
        assert!(
            source
                .read_range(0, 1)
                .unwrap_err()
                .to_string()
                .contains("invalidated")
        );
        assert_eq!(source.metrics.requests, 2);
    }
}

#[cfg(test)]
mod range_prefetch_tests {
    use super::*;

    fn bytes(offset: u64, length: u64) -> Vec<u8> {
        (offset..offset + length).map(|n| (n % 251) as u8).collect()
    }

    fn source(url: &str, capacity: usize) -> RangeSource {
        RangeSource {
            client: Client::builder()
                .timeout(Duration::from_secs(3))
                .build()
                .unwrap(),
            url: reqwest::Url::parse(url).unwrap(),
            length: 4096,
            etag: "\"fixed\"".into(),
            source_id: "prefetch-test".into(),
            metrics: RemoteMetrics {
                cache_capacity_bytes: capacity,
                ..Default::default()
            },
            limits: RemoteLimits {
                max_range_bytes: 1024,
                ..Default::default()
            },
            cancel: Arc::new(AtomicBool::new(false)),
            cache: Default::default(),
            logical_reads: 0,
            invalidated: false,
        }
    }

    fn cache(source: &mut RangeSource, offset: u64, length: u64) {
        source
            .cache
            .push_back(((offset, length), bytes(offset, length)));
        source.metrics.cache_resident_bytes += length as usize + 128;
        source.metrics.cache_peak_bytes = source.metrics.cache_resident_bytes;
        assert!(source.metrics.cache_resident_bytes <= source.cache_byte_capacity());
    }

    struct Server {
        url: String,
        ranges: Arc<Mutex<Vec<(u64, u64)>>>,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }
    impl Server {
        fn new(fail_at: Option<u64>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}/raw", listener.local_addr().unwrap());
            let stop = Arc::new(AtomicBool::new(false));
            let ranges = Arc::new(Mutex::new(Vec::new()));
            let thread_stop = stop.clone();
            let thread_ranges = ranges.clone();
            let thread = thread::spawn(move || {
                while !thread_stop.load(Ordering::Relaxed) {
                    let mut stream = match listener.accept() {
                        Ok((stream, _)) => stream,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                        Err(e) => panic!("{e}"),
                    };
                    stream
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") {
                        let mut byte = [0];
                        stream.read_exact(&mut byte).unwrap();
                        request.push(byte[0]);
                        assert!(request.len() <= 8192);
                    }
                    let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                    assert!(request.starts_with("get "));
                    assert!(request.contains("if-match: \"fixed\""));
                    let range = request
                        .lines()
                        .find_map(|line| line.strip_prefix("range: bytes="))
                        .unwrap();
                    let (begin, end) = range.split_once('-').unwrap();
                    let offset: u64 = begin.parse().unwrap();
                    let end: u64 = end.parse().unwrap();
                    let length = end - offset + 1;
                    thread_ranges.lock().unwrap().push((offset, length));
                    if fail_at == Some(offset) {
                        stream.write_all(b"HTTP/1.1 412 Precondition Failed\r\nContent-Length:0\r\nConnection:close\r\n\r\n").unwrap();
                    } else {
                        let response = format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Length:{length}\r\nContent-Range:bytes {offset}-{end}/4096\r\nETag:\"fixed\"\r\nConnection:close\r\n\r\n"
                        );
                        stream.write_all(response.as_bytes()).unwrap();
                        stream.write_all(&bytes(offset, length)).unwrap();
                    }
                }
            });
            Self {
                url,
                ranges,
                stop,
                thread: Some(thread),
            }
        }
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            self.thread.take().unwrap().join().unwrap();
        }
    }

    #[test]
    fn guarded_noops_do_not_mutate_cache_or_consume_budgets() {
        let cancel = AtomicBool::new(false);
        let mut source = source("http://127.0.0.1:9/raw", 512);
        cache(&mut source, 512, 32);
        cache(&mut source, 1024, 32);
        let original = source.cache.clone();
        let demands = [(0, 64), (64, 64)];
        for scratch in [0, PREFETCH_CONTROL_BYTES + HTTP_SCRATCH_BYTES + 63] {
            assert!(
                !source
                    .prefetch_exact_ranges(&demands, scratch, &cancel)
                    .unwrap()
                    .admitted
            );
        }
        // Two isolated demands cannot save a request, nor can all-cache hits.
        for demands in [&[(0, 32), (128, 32)][..], &[(512, 16), (528, 16)][..]] {
            assert!(
                !source
                    .prefetch_exact_ranges(demands, 1 << 20, &cancel)
                    .unwrap()
                    .admitted
            );
        }
        source.limits.max_download_bytes = 127;
        assert!(
            !source
                .prefetch_exact_ranges(&demands, 1 << 20, &cancel)
                .unwrap()
                .admitted
        );
        source.limits.max_download_bytes = 4096;
        source.metrics.requests = source.limits.max_requests;
        assert!(
            !source
                .prefetch_exact_ranges(&demands, 1 << 20, &cancel)
                .unwrap()
                .admitted
        );
        source.metrics.requests = 0;
        source.logical_reads = source.limits.max_requests * 16 - 2;
        assert!(
            !source
                .prefetch_exact_ranges(&demands, 1 << 20, &cancel)
                .unwrap()
                .admitted
        );
        assert_eq!(source.cache, original);
        assert_eq!(source.metrics.get_requests, 0);
        assert_eq!(source.metrics.cache_hits, 0);
        assert_eq!(source.metrics.cache_misses, 0);
        assert_eq!(source.metrics.cache_evictions, 0);
        // Shared cached superset is charged in full, not only selected bytes.
        source.logical_reads = 0;
        source.metrics.cache_capacity_bytes = 400;
        assert!(
            !source
                .prefetch_exact_ranges(&[(0, 64), (64, 64), (512, 1)], 1 << 20, &cancel)
                .unwrap()
                .admitted
        );
        source.metrics.cache_capacity_bytes = 0;
        assert!(
            !source
                .prefetch_exact_ranges(&demands, 1 << 20, &cancel)
                .unwrap()
                .admitted
        );
    }

    #[test]
    fn invalid_demands_and_cancellation_fail_even_without_cache() {
        let mut source = source("http://127.0.0.1:9/raw", 0);
        let cancel = AtomicBool::new(false);
        for demands in [
            vec![(0, 0)],
            vec![(0, 16), (8, 16)],
            vec![(64, 1), (0, 1)],
            vec![(4090, 8)],
            vec![(0, 1025)],
            vec![(u64::MAX, 1)],
            vec![(0, 1); 129],
        ] {
            assert!(
                source
                    .prefetch_exact_ranges(&demands, 1 << 20, &cancel)
                    .is_err()
            );
        }
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(
            source
                .prefetch_exact_ranges(&[], 0, &cancel)
                .unwrap_err()
                .to_string(),
            "cancelled"
        );
        cancel.store(false, Ordering::Relaxed);
        source.cancel.store(true, Ordering::Relaxed);
        assert_eq!(
            source
                .prefetch_exact_ranges(&[], 0, &cancel)
                .unwrap_err()
                .to_string(),
            "cancelled"
        );
        source.cancel.store(false, Ordering::Relaxed);
        source.invalidated = true;
        assert!(
            source
                .prefetch_exact_ranges(&[], 0, &cancel)
                .unwrap_err()
                .to_string()
                .contains("invalidated")
        );
        assert_eq!(source.metrics.requests, 0);
    }

    #[test]
    fn adjacent_missing_demands_and_singletons_survive_protected_lru() {
        let server = Server::new(None);
        let mut source = source(&server.url, 1024);
        cache(&mut source, 512, 128);
        cache(&mut source, 800, 200);
        cache(&mut source, 0, 32);
        cache(&mut source, 1100, 100);
        let demands = [
            (0, 16),
            (16, 16),
            (128, 64),
            (192, 64),
            (384, 32),
            (512, 64),
            (576, 64),
        ];
        let cancel = AtomicBool::new(false);
        let result = source
            .prefetch_exact_ranges(&demands, 1 << 20, &cancel)
            .unwrap();
        assert!(result.admitted);
        assert_eq!(result.planned_request_savings, 1);
        assert_eq!(result.protected_cache_charge_bytes, 416);
        assert_eq!(result.new_cache_charge_bytes, 416);
        assert_eq!(
            source.cache.iter().map(|entry| entry.0).collect::<Vec<_>>(),
            [(512, 128), (0, 32), (128, 128), (384, 32)]
        );
        assert_eq!(result.events.len(), 2);
        assert!(
            result
                .events
                .iter()
                .all(|event| event.2 >= result.planning_ms && event.3 >= 0.0)
        );
        for (offset, length) in demands {
            assert_eq!(
                source.read_range(offset, length).unwrap(),
                bytes(offset, length)
            );
        }
        assert_eq!(*server.ranges.lock().unwrap(), [(128, 128), (384, 32)]);
        assert_eq!(source.metrics.received_bytes, 160);
        assert_eq!(source.metrics.get_requests, 2);
        assert_eq!(source.metrics.cache_hits, 7);
        assert_eq!(source.metrics.cache_evictions, 2);
        assert_eq!(source.metrics.cache_resident_bytes, 832);
        assert!(source.metrics.cache_peak_bytes <= source.cache_byte_capacity());
    }

    #[test]
    fn scratch_and_range_caps_never_split_future_demands() {
        let server = Server::new(None);
        let cancel = AtomicBool::new(false);
        let demands = [(0, 64), (64, 32), (96, 64), (160, 32)];
        for (range_limit, scratch_response) in [(96, 1024), (1024, 96)] {
            let mut source = source(&server.url, 1024);
            source.limits.max_range_bytes = range_limit;
            let result = source
                .prefetch_exact_ranges(
                    &demands,
                    PREFETCH_CONTROL_BYTES + HTTP_SCRATCH_BYTES + scratch_response,
                    &cancel,
                )
                .unwrap();
            assert!(result.admitted);
            assert_eq!(result.planned_request_savings, 2);
            assert_eq!(
                result
                    .events
                    .iter()
                    .map(|event| (event.0, event.1))
                    .collect::<Vec<_>>(),
                [(0, 96), (96, 96)]
            );
            for (offset, length) in demands {
                assert_eq!(
                    source.read_range(offset, length).unwrap(),
                    bytes(offset, length)
                );
            }
            assert_eq!(source.metrics.get_requests, 2);
        }
        // Two cached pieces are not a whole cached demand: preserve the
        // existing containment semantics rather than inventing slice assembly.
        let mut source = source(&server.url, 1024);
        cache(&mut source, 0, 32);
        cache(&mut source, 32, 32);
        let result = source
            .prefetch_exact_ranges(&[(0, 64), (64, 64)], 1 << 20, &cancel)
            .unwrap();
        assert_eq!(
            result
                .events
                .iter()
                .map(|event| (event.0, event.1))
                .collect::<Vec<_>>(),
            [(0, 128)]
        );
        assert_eq!(source.metrics.received_bytes, 128);
    }

    #[test]
    fn later_prefetch_failure_quarantines_all_prepared_bytes() {
        let server = Server::new(Some(128));
        let mut source = source(&server.url, 1024);
        let cancel = AtomicBool::new(false);
        assert!(
            source
                .prefetch_exact_ranges(&[(0, 32), (32, 32), (128, 32), (160, 32)], 1 << 20, &cancel)
                .is_err()
        );
        assert_eq!(*server.ranges.lock().unwrap(), [(0, 64), (128, 64)]);
        assert_eq!(source.metrics.requests, 2);
        assert_eq!(source.metrics.received_bytes, 64);
        assert_eq!(source.metrics.failed_requests, 1);
        assert_eq!(source.metrics.cache_resident_bytes, 0);
        assert!(source.cache.is_empty());
        assert!(
            source
                .prefetch_exact_ranges(&[], 0, &cancel)
                .unwrap_err()
                .to_string()
                .contains("invalidated")
        );
        assert!(source.read_range(0, 1).is_err());
        assert_eq!(source.metrics.requests, 2);
    }
}
