//! One-shot representation selection under an explicit owner-provisioned proof.
//! An attestation is trusted provenance, not a cryptographic proof of equality.
use crate::{
    model::{Grid, check_cancel},
    ordered_source::{self, OrderedPolygon, OrderedRequest, OrderedWindow},
    source::{RawRasterMetadata, RawScalarType, SourceSpec},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{io::Write, sync::atomic::AtomicBool, time::Instant};

pub const SCHEMA: &str = "skarve_serving_profile_v1";
pub const MAX_PROFILE_BYTES: usize = 65_536;
const PROFILE_WORKING_BYTES: usize = 4 << 20;
const SOURCE_RETAINED_BYTES: usize = 16 << 20;
const SOURCE_OPEN_BYTES: usize = 32 << 20;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedGrid {
    pub width: usize,
    pub height: usize,
    pub transform_f64_bits: [String; 6],
    pub crs: String,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedBand {
    pub scalar_type: RawScalarType,
    pub nodata_f64_bits: Option<String>,
    pub scale_f64_bits: String,
    pub offset_f64_bits: String,
    pub unit: Option<String>,
    pub mask_flags: u32,
    pub original_band_index: usize,
    pub description: String,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedRawMetadata {
    pub bands: Vec<ExpectedBand>,
    pub pixel_convention: String,
    pub source_band_count: usize,
    #[serde(default)]
    pub source_overview: Option<usize>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedView {
    pub grid: ExpectedGrid,
    pub raw_metadata: ExpectedRawMetadata,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    pub spec: SourceSpec,
    pub expected: ExpectedView,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectAttestation {
    pub sha256: String,
    pub byte_length: u64,
    pub selector_sha256: String,
    pub expected_view_sha256: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attestation {
    pub kind: String,
    pub verifier: String,
    pub receipt_sha256: String,
    pub checks: Vec<String>,
    pub samples_sha256: String,
    pub masks_sha256: String,
    pub interpretation_sha256: String,
    pub cells_per_band: u64,
    pub direct: ObjectAttestation,
    pub accelerated: ObjectAttestation,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Eligibility {
    pub enabled: bool,
    pub access_classes: Vec<String>,
    /// Exact logical band set. Requested output order is never changed.
    pub bands: Vec<usize>,
    pub polygons: [usize; 2],
    pub windows: [usize; 2],
    pub selected_cells: [u64; 2],
    pub envelope_width: [usize; 2],
    pub envelope_height: [usize; 2],
    pub envelope_cells: [u64; 2],
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServingProfile {
    pub schema: String,
    pub profile_id: String,
    pub view_id: String,
    pub numerical_policy: String,
    pub direct: Candidate,
    pub accelerated: Candidate,
    pub attestation: Attestation,
    pub accelerated_when: Eligibility,
}
#[derive(Debug, Serialize)]
pub struct RequestFacts {
    pub selected_bands: Vec<usize>,
    pub polygon_count: usize,
    pub window_count: usize,
    /// Counts repetitions across polygons/logical windows, once per spatial cell.
    pub selected_cells: u64,
    pub selected_contributions: u64,
    /// Union of nonempty logical windows; not a physical decoder footprint.
    pub window_envelope: Option<[usize; 4]>,
    pub envelope_cells: u64,
}

fn lower_hex(value: &str, size: usize) -> bool {
    value.len() == size
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn bits(value: &str) -> Result<u64> {
    ensure!(
        lower_hex(value, 16),
        "expected view requires16 lowercase hexadecimal float bits"
    );
    Ok(u64::from_str_radix(value, 16)?)
}
fn bounded_label(value: &str, limit: usize) -> bool {
    !value.is_empty() && value.len() <= limit && !value.chars().any(char::is_control)
}
fn hex64(value: &str) -> Result<()> {
    ensure!(
        lower_hex(value, 64),
        "profile SHA256 must be64 lowercase hexadecimal characters"
    );
    Ok(())
}
/// SHA256 over compact JSON with recursively sorted object keys, preserving arrays.
pub fn canonical_sha256(value: &Value) -> Result<String> {
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(object) => {
                let mut keys = object.keys().collect::<Vec<_>>();
                keys.sort_unstable();
                Value::Object(
                    keys.into_iter()
                        .map(|k| (k.clone(), sorted(&object[k])))
                        .collect(),
                )
            }
            Value::Array(values) => Value::Array(values.iter().map(sorted).collect()),
            _ => value.clone(),
        }
    }
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&sorted(value))?)
    ))
}
pub fn selector_sha256(spec: &SourceSpec) -> Result<String> {
    canonical_sha256(
        &json!({"format":spec.format,"variable":spec.variable,"overview":spec.overview,
        "crs":spec.crs,"longitude_shift":spec.longitude_shift,"bands":spec.bands}),
    )
}
pub fn expected_view(grid: &Grid, raw: &RawRasterMetadata) -> ExpectedView {
    ExpectedView {
        grid: ExpectedGrid {
            width: grid.width,
            height: grid.height,
            transform_f64_bits: grid.transform.map(|v| format!("{:016x}", v.to_bits())),
            crs: grid.crs.clone(),
        },
        raw_metadata: ExpectedRawMetadata {
            bands: raw
                .bands
                .iter()
                .map(|b| ExpectedBand {
                    scalar_type: b.scalar_type,
                    nodata_f64_bits: b.nodata_f64_bits.map(|v| format!("{v:016x}")),
                    scale_f64_bits: format!("{:016x}", b.scale_f64_bits),
                    offset_f64_bits: format!("{:016x}", b.offset_f64_bits),
                    unit: b.unit.clone(),
                    mask_flags: b.mask_flags,
                    original_band_index: b.original_band_index,
                    description: b.description.clone(),
                })
                .collect(),
            pixel_convention: raw.pixel_convention.clone(),
            source_band_count: raw.source_band_count,
            source_overview: raw.source_overview,
        },
    }
}
pub fn expected_view_sha256(view: &ExpectedView) -> Result<String> {
    canonical_sha256(&serde_json::to_value(view)?)
}
pub fn interpretation_sha256(view: &ExpectedView) -> Result<String> {
    let mut value = serde_json::to_value(view)?;
    let raw = value["raw_metadata"]
        .as_object_mut()
        .context("invalid expected raw metadata")?;
    raw.remove("source_band_count");
    raw.remove("source_overview");
    for band in raw
        .get_mut("bands")
        .and_then(Value::as_array_mut)
        .context("invalid expected bands")?
    {
        band.as_object_mut()
            .context("invalid expected band")?
            .remove("original_band_index");
    }
    canonical_sha256(&value)
}

impl ExpectedView {
    fn validate(&self) -> Result<Grid> {
        let mut transform = [0.; 6];
        for (value, encoded) in transform.iter_mut().zip(&self.grid.transform_f64_bits) {
            *value = f64::from_bits(bits(encoded)?);
        }
        let grid = Grid {
            width: self.grid.width,
            height: self.grid.height,
            transform,
            crs: self.grid.crs.clone(),
        };
        grid.validate()?;
        let raw = &self.raw_metadata;
        ensure!(
            (1..=64).contains(&raw.bands.len())
                && raw.source_band_count >= raw.bands.len()
                && raw.source_band_count <= 65536
                && raw.source_overview.is_none_or(|v| v < 64)
                && ["Area", "Point", "unspecified"].contains(&raw.pixel_convention.as_str()),
            "invalid expected source metadata"
        );
        for (i, band) in raw.bands.iter().enumerate() {
            ensure!(
                matches!(
                    band.scalar_type,
                    RawScalarType::Float32 | RawScalarType::Float64
                ),
                "ordered profile requires original Float32/Float64 bands"
            );
            ensure!(
                f64::from_bits(bits(&band.scale_f64_bits)?).is_finite()
                    && f64::from_bits(bits(&band.offset_f64_bits)?).is_finite(),
                "invalid expected scale/offset"
            );
            if let Some(value) = &band.nodata_f64_bits {
                bits(value)?;
            }
            ensure!(
                band.unit.as_ref().is_none_or(|v| v.len() <= 1024)
                    && band.description.len() <= 4096
                    && band.mask_flags & !15 == 0
                    && band.original_band_index < raw.source_band_count
                    && raw.bands[..i]
                        .iter()
                        .all(|b| b.original_band_index != band.original_band_index),
                "invalid expected band interpretation"
            );
        }
        Ok(grid)
    }
}

struct SizeCounter {
    bytes: usize,
    limit: usize,
}
impl Write for SizeCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .ok_or_else(|| std::io::Error::other("profile size overflow"))?;
        if self.bytes > self.limit {
            return Err(std::io::Error::other("serving profile exceeds64KiB"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub fn parse(value: &Value) -> Result<ServingProfile> {
    // Count before cloning/deserializing; serialization never grows a buffer.
    serde_json::to_writer(
        SizeCounter {
            bytes: 0,
            limit: MAX_PROFILE_BYTES,
        },
        value,
    )?;
    Ok(serde_json::from_value(value.clone())?)
}

fn candidate_valid(
    candidate: &Candidate,
    pin: &ObjectAttestation,
    interpretation: &str,
) -> Result<Grid> {
    crate::source::validate_source_spec(&candidate.spec)?;
    let grid = candidate.expected.validate()?;
    let identity = candidate
        .spec
        .identity
        .as_ref()
        .context("profile candidates require explicit content identity")?;
    for value in [&pin.sha256, &pin.selector_sha256, &pin.expected_view_sha256] {
        hex64(value)?;
    }
    ensure!(
        pin.byte_length > 0
            && identity.sha256 == pin.sha256
            && identity.byte_length == pin.byte_length,
        "profile source content pin differs from owner attestation"
    );
    ensure!(
        selector_sha256(&candidate.spec)? == pin.selector_sha256,
        "profile source view selector differs from owner attestation"
    );
    ensure!(
        expected_view_sha256(&candidate.expected)? == pin.expected_view_sha256,
        "profile expected source view differs from owner attestation"
    );
    ensure!(
        interpretation_sha256(&candidate.expected)? == interpretation,
        "profile candidates do not share the attested interpretation"
    );
    Ok(grid)
}
impl ServingProfile {
    fn validate(&self) -> Result<Grid> {
        ensure!(
            self.schema == SCHEMA
                && bounded_label(&self.profile_id, 128)
                && bounded_label(&self.view_id, 128)
                && self.numerical_policy == ordered_source::POLICY,
            "unsupported serving profile/schema/policy"
        );
        let proof = &self.attestation;
        ensure!(
            proof.kind == "owner_verified_full_view_v1" && bounded_label(&proof.verifier, 256),
            "profile requires an explicit owner-verified full-view attestation"
        );
        let checks = ["typed_sample_bits", "mask_bytes", "full_interpretation"];
        ensure!(
            proof.checks.len() == checks.len()
                && checks
                    .iter()
                    .all(|check| proof.checks.iter().any(|v| v == check)),
            "profile attestation must declare typed sample, mask and interpretation checks"
        );
        for value in [
            &proof.receipt_sha256,
            &proof.samples_sha256,
            &proof.masks_sha256,
            &proof.interpretation_sha256,
        ] {
            hex64(value)?;
        }
        let direct = candidate_valid(&self.direct, &proof.direct, &proof.interpretation_sha256)?;
        let accelerated = candidate_valid(
            &self.accelerated,
            &proof.accelerated,
            &proof.interpretation_sha256,
        )?;
        ensure!(
            direct == accelerated
                && proof.cells_per_band == direct.width as u64 * direct.height as u64,
            "profile candidates are not the same complete attested view"
        );
        let n = self.direct.expected.raw_metadata.bands.len();
        let rule = &self.accelerated_when;
        ensure!(
            rule.access_classes.len() <= 8
                && rule
                    .access_classes
                    .iter()
                    .enumerate()
                    .all(|(i, v)| bounded_label(v, 64)
                        && v != "unknown"
                        && !rule.access_classes[..i].contains(v)),
            "invalid profile access classes"
        );
        ensure!(
            !rule.bands.is_empty()
                && rule.bands.len() <= n
                && rule
                    .bands
                    .iter()
                    .enumerate()
                    .all(|(i, &b)| b < n && !rule.bands[..i].contains(&b)),
            "invalid profile eligible band set"
        );
        for (range, cap) in [
            (rule.polygons, 256),
            (rule.windows, 4096),
            (rule.envelope_width, direct.width),
            (rule.envelope_height, direct.height),
        ] {
            ensure!(
                range[0] <= range[1] && range[1] <= cap,
                "invalid profile eligibility bounds"
            );
        }
        ensure!(
            rule.selected_cells[0] <= rule.selected_cells[1]
                && rule.selected_cells[1] <= 268435456
                && rule.envelope_cells[0] <= rule.envelope_cells[1]
                && rule.envelope_cells[1] <= proof.cells_per_band,
            "invalid profile eligibility work bounds"
        );
        Ok(direct)
    }
}

fn add_bytes(total: &mut usize, count: usize, width: usize) -> Result<()> {
    *total = total
        .checked_add(
            count
                .checked_mul(width)
                .context("profile request size overflow")?,
        )
        .context("profile request size overflow")?;
    Ok(())
}
fn request_facts(
    request: &OrderedRequest,
    view: &ExpectedView,
    grid: &Grid,
    cancel: &AtomicBool,
) -> Result<RequestFacts> {
    ordered_source::reservation_bytes(request)?;
    let count = view.raw_metadata.bands.len();
    ensure!(
        !request.polygons.is_empty()
            && request.polygons.len() <= 256
            && request.bands.len() <= count,
        "ordered profile request count exceeds bound"
    );
    let selected_bands = if request.bands.is_empty() {
        (0..count).collect::<Vec<_>>()
    } else {
        request.bands.clone()
    };
    ensure!(
        selected_bands
            .iter()
            .enumerate()
            .all(|(i, &b)| b < count && !selected_bands[..i].contains(&b)),
        "invalid ordered profile band selection"
    );
    ensure!(
        request.nodata.is_none(),
        "serving profile v1 does not accept NoData overrides"
    );
    let mut memory = std::mem::size_of::<OrderedRequest>();
    add_bytes(
        &mut memory,
        request.polygons.capacity(),
        std::mem::size_of::<OrderedPolygon>(),
    )?;
    add_bytes(
        &mut memory,
        request.bands.capacity(),
        std::mem::size_of::<usize>(),
    )?;
    if let Some(values) = &request.nodata {
        add_bytes(
            &mut memory,
            values.capacity(),
            std::mem::size_of::<Option<f64>>(),
        )?;
    }
    let mut windows = 0usize;
    let mut selected = 0u64;
    let mut envelope: Option<[usize; 4]> = None;
    for (p, polygon) in request.polygons.iter().enumerate() {
        check_cancel(cancel)?;
        ensure!(
            bounded_label(&polygon.id, 128)
                && request.polygons[..p].iter().all(|old| old.id != polygon.id),
            "invalid or duplicate ordered profile polygon id"
        );
        windows = windows
            .checked_add(polygon.windows.len())
            .context("profile window overflow")?;
        ensure!(
            windows <= 4096,
            "ordered profile logical window count exceeds bound"
        );
        add_bytes(&mut memory, polygon.id.capacity(), 1)?;
        add_bytes(
            &mut memory,
            polygon.windows.capacity(),
            std::mem::size_of::<OrderedWindow>(),
        )?;
        for window in &polygon.windows {
            let [x, y, w, h] = window.window;
            let cells = w
                .checked_mul(h)
                .context("profile logical window overflow")?;
            ensure!(
                w > 0
                    && h > 0
                    && cells <= u32::MAX as usize
                    && x.checked_add(w).is_some_and(|end| end <= grid.width)
                    && y.checked_add(h).is_some_and(|end| end <= grid.height),
                "profile window lies outside attested view"
            );
            ensure!(
                window.indexes.is_some() != window.runs.is_some(),
                "profile requires exactly one indexes or runs selection"
            );
            let before = selected;
            if let Some(indexes) = &window.indexes {
                add_bytes(&mut memory, indexes.capacity(), std::mem::size_of::<u32>())?;
                ensure!(
                    memory <= request.budget.planning_bytes,
                    "profile request exceeds planning budget"
                );
                selected = selected
                    .checked_add(indexes.len() as u64)
                    .context("profile selection overflow")?;
                ensure!(
                    selected <= request.budget.max_contributions,
                    "profile selected work exceeds budget"
                );
                let mut previous = None;
                for (i, &index) in indexes.iter().enumerate() {
                    if i % 1024 == 0 {
                        check_cancel(cancel)?;
                    }
                    ensure!(
                        (index as usize) < cells && previous.is_none_or(|old| old < index),
                        "profile indexes must be ascending and in bounds"
                    );
                    previous = Some(index);
                }
            } else if let Some(runs) = &window.runs {
                add_bytes(
                    &mut memory,
                    runs.capacity(),
                    std::mem::size_of::<[u32; 2]>(),
                )?;
                ensure!(
                    memory <= request.budget.planning_bytes,
                    "profile request exceeds planning budget"
                );
                let mut previous = 0;
                for (i, &[start, end]) in runs.iter().enumerate() {
                    if i % 1024 == 0 {
                        check_cancel(cancel)?;
                    }
                    ensure!(
                        start < end && end as usize <= cells && (i == 0 || start >= previous),
                        "profile runs must be sorted disjoint and in bounds"
                    );
                    selected = selected
                        .checked_add((end - start) as u64)
                        .context("profile selection overflow")?;
                    ensure!(
                        selected <= request.budget.max_contributions,
                        "profile selected work exceeds budget"
                    );
                    previous = end;
                }
            }
            if selected > before {
                envelope = Some(match envelope {
                    None => [x, y, x + w, y + h],
                    Some([x0, y0, x1, y1]) => [x0.min(x), y0.min(y), x1.max(x + w), y1.max(y + h)],
                });
            }
        }
    }
    ensure!(
        memory <= request.budget.planning_bytes,
        "profile request exceeds planning budget"
    );
    let contributions = selected
        .checked_mul(selected_bands.len() as u64)
        .context("profile contribution overflow")?;
    ensure!(
        contributions <= request.budget.max_contributions,
        "profile contributions exceed budget"
    );
    let window_envelope = envelope.map(|[x0, y0, x1, y1]| [x0, y0, x1 - x0, y1 - y0]);
    Ok(RequestFacts {
        selected_bands,
        polygon_count: request.polygons.len(),
        window_count: windows,
        selected_cells: selected,
        selected_contributions: contributions,
        window_envelope,
        envelope_cells: window_envelope.map_or(0, |r| r[2] as u64 * r[3] as u64),
    })
}
fn within<T: PartialOrd>(value: T, range: [T; 2]) -> bool {
    value >= range[0] && value <= range[1]
}
fn eligible(rule: &Eligibility, facts: &RequestFacts, access: &str) -> bool {
    let envelope = facts.window_envelope.unwrap_or([0; 4]);
    rule.enabled
        && access != "unknown"
        && rule.access_classes.iter().any(|v| v == access)
        && facts.selected_bands.len() == rule.bands.len()
        && facts.selected_bands.iter().all(|b| rule.bands.contains(b))
        && within(facts.polygon_count, rule.polygons)
        && within(facts.window_count, rule.windows)
        && within(facts.selected_cells, rule.selected_cells)
        && within(envelope[2], rule.envelope_width)
        && within(envelope[3], rule.envelope_height)
        && within(facts.envelope_cells, rule.envelope_cells)
}

/// Executes and drops one selected ordinary source. No reader handle escapes.
#[allow(clippy::too_many_arguments)]
pub fn execute(
    profile_value: &Value,
    request: &OrderedRequest,
    policy: &str,
    view_id: &str,
    access_class: Option<&str>,
    available: usize,
    cancel: &AtomicBool,
) -> Result<Value> {
    let started = Instant::now();
    check_cancel(cancel)?;
    let working = ordered_source::reservation_bytes(request)?;
    let required = SOURCE_OPEN_BYTES
        .max(
            working
                .checked_add(SOURCE_RETAINED_BYTES)
                .context("profile reservation overflow")?,
        )
        .checked_add(PROFILE_WORKING_BYTES)
        .context("profile reservation overflow")?;
    ensure!(
        available >= required,
        "insufficient session memory for one-shot profile execution"
    );
    let profile = parse(profile_value)?;
    let grid = profile.validate()?;
    ensure!(
        policy == ordered_source::POLICY
            && policy == profile.numerical_policy
            && view_id == profile.view_id,
        "profile query policy/view differs from attested contract"
    );
    let access = access_class.unwrap_or("unknown");
    ensure!(
        bounded_label(access, 64),
        "profile access class exceeds bound"
    );
    let facts = request_facts(request, &profile.direct.expected, &grid, cancel)?;
    let accelerated = eligible(&profile.accelerated_when, &facts, access);
    let (role, candidate, pin) = if accelerated {
        (
            "accelerated",
            &profile.accelerated,
            &profile.attestation.accelerated,
        )
    } else {
        ("direct", &profile.direct, &profile.attestation.direct)
    };
    check_cancel(cancel)?;
    let routing_ms = started.elapsed().as_secs_f64() * 1000.;
    let opened = Instant::now();
    // There is exactly one open call and deliberately no fallback on its error.
    let source = crate::io::open_source(&candidate.spec, cancel)?;
    ensure!(
        source.retained_memory_bound() <= SOURCE_RETAINED_BYTES,
        "profile selected source exceeds retained reservation"
    );
    let raw = source
        .raw_metadata()
        .context("profile selected source lacks raw metadata")?;
    let actual = expected_view(&source.metadata().grid, raw);
    ensure!(
        expected_view_sha256(&actual)? == pin.expected_view_sha256,
        "opened source interpretation differs from attested expected view"
    );
    let source_open_and_check_ms = opened.elapsed().as_secs_f64() * 1000.;
    let mut result = ordered_source::execute(source.as_ref(), request, cancel)?;
    let identity_policy = match candidate
        .spec
        .identity
        .as_ref()
        .expect("validated identity")
        .policy
    {
        crate::source::VerificationPolicy::Verify => "verify",
        crate::source::VerificationPolicy::TrustedManifest => "trusted_manifest",
    };
    drop(source);
    check_cancel(cancel)?;
    result["routing"] = json!({"schema":SCHEMA,"profile_id":profile.profile_id,"view_id":profile.view_id,
        "selected":role,"reason":if accelerated {"eligible_actual_request"}else{"outside_accelerated_rule"},
        "access_class":access,"access_class_authority":"caller_declared_no_runtime_probe", "facts":facts,
        "numerical_policy":policy,"source_sha256":pin.sha256,"source_byte_length":pin.byte_length,
        "source_verification_policy":identity_policy,"attestation_authority":"trusted_owner_receipt_not_runtime_equivalence_proof",
        "receipt_sha256":profile.attestation.receipt_sha256,"verifier":profile.attestation.verifier,
        "unselected_source_opened":false,"source_closed":true,"routing_ms":routing_ms,
        "source_open_and_check_ms":source_open_and_check_ms,"complete_profile_ms":started.elapsed().as_secs_f64()*1000.,
        "reservation_bytes":required});
    Ok(result)
}
