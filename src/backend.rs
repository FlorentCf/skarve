//! Backend selection is subordinate to the accepted numerical and execution contract.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const NATIVE_POLICY: &str = "native_grid_planar_fractional";
pub const EXACTEXTRACT_POLICY: &str = "exactextract_fractional_v030";
pub const EXACTEXTRACT_RASTERIO_POLICY: &str = "exactextract_rasterio_v030";
pub const FIELDS: [&str; 5] = ["sum", "support", "mean", "min", "max"];
pub const REQUEST_FIELDS: [&str; 5] = [
    "backend",
    "numerical_policy",
    "accepted_policies",
    "execution_envelope",
    "backend_options",
];

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Backend {
    Native,
    Exactextract,
}

#[derive(Clone)]
pub struct Decision {
    pub selected: Backend,
    pub provenance: Value,
    pub options: EeOptions,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EeOptions {
    /// Set only by numerical-policy resolution, never by backend_options.
    #[serde(skip)]
    pub rasterio_compatible: bool,
    pub strategy: String,
    pub max_cells_in_memory: usize,
    pub window_bytes: usize,
    pub output_bytes: usize,
    pub max_windows: usize,
    pub decoded_bytes: u64,
}
impl Default for EeOptions {
    fn default() -> Self {
        Self {
            rasterio_compatible: false,
            strategy: "raster-sequential".into(),
            max_cells_in_memory: 262144,
            window_bytes: 64 << 20,
            output_bytes: 16 << 20,
            max_windows: 16384,
            decoded_bytes: 2 << 30,
        }
    }
}
impl EeOptions {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            ["raster-sequential", "feature-sequential"].contains(&self.strategy.as_str()),
            "unknown exactextract strategy"
        );
        ensure!(
            (1..=4_194_304).contains(&self.max_cells_in_memory),
            "exactextract max_cells_in_memory must be in 1..4194304"
        );
        ensure!(
            (4096..=256 << 20).contains(&self.window_bytes),
            "exactextract window_bytes must be in 4096..268435456"
        );
        ensure!(
            (4096..=32 << 20).contains(&self.output_bytes),
            "exactextract output_bytes must be in 4096..33554432"
        );
        ensure!(
            (1..=1_000_000).contains(&self.max_windows)
                && (1..=16_u64 << 30).contains(&self.decoded_bytes),
            "invalid exactextract cumulative read budget"
        );
        Ok(())
    }
}

pub fn resolve(v: &Value, prepared: bool) -> Result<Decision> {
    let requested = match v.get("backend") {
        None => "native",
        Some(n) => n
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("backend must be a string"))?,
    };
    ensure!(
        ["native", "exactextract", "auto"].contains(&requested),
        "unknown execution backend"
    );
    ensure!(
        v.get("numerical_policy").is_none() || v.get("accepted_policies").is_none(),
        "numerical_policy and accepted_policies are mutually exclusive"
    );
    let accepted: Vec<String> = if let Some(n) = v.get("numerical_policy") {
        vec![
            n.as_str()
                .ok_or_else(|| anyhow::anyhow!("numerical_policy must be a string"))?
                .to_owned(),
        ]
    } else if let Some(n) = v.get("accepted_policies") {
        ensure!(
            n.as_array().is_some_and(|a| !a.is_empty() && a.len() <= 3),
            "accepted_policies requires one to three policy names"
        );
        serde_json::from_value(n.clone())?
    } else {
        vec![
            if requested == "exactextract" {
                EXACTEXTRACT_POLICY
            } else {
                NATIVE_POLICY
            }
            .into(),
        ]
    };
    ensure!(
        !accepted.is_empty()
            && accepted.len() <= 3
            && accepted.iter().all(|n| [
                NATIVE_POLICY,
                EXACTEXTRACT_POLICY,
                EXACTEXTRACT_RASTERIO_POLICY
            ]
            .contains(&n.as_str())),
        "unknown or empty accepted numerical policies"
    );
    let envelope = match v.get("execution_envelope") {
        None => "embedded_cooperative",
        Some(n) => n
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("execution_envelope must be a string"))?,
    };
    ensure!(
        envelope == "embedded_cooperative",
        "requested execution envelope is unavailable: installed embedded backends provide cooperative cancellation, not process isolation or hard termination"
    );
    let mut options: EeOptions = v
        .get("backend_options")
        .map(|n| serde_json::from_value(n.clone()))
        .transpose()?
        .unwrap_or_default();
    options.validate()?;
    let native_allowed = accepted.iter().any(|p| p == NATIVE_POLICY);
    let selected = match requested {
        "native" => Backend::Native,
        "exactextract" => Backend::Exactextract,
        // A conservative explicit-policy gate. This is not a polygon-count heuristic.
        "auto" if native_allowed => Backend::Native,
        _ => Backend::Exactextract,
    };
    let policy = if selected == Backend::Native {
        NATIVE_POLICY
    } else if accepted.iter().any(|p| p == EXACTEXTRACT_POLICY) {
        // Preserve the legacy preference if both optional policies are accepted.
        EXACTEXTRACT_POLICY
    } else {
        EXACTEXTRACT_RASTERIO_POLICY
    };
    ensure!(
        accepted.iter().any(|p| p == policy),
        "selected backend conflicts with the accepted numerical policy"
    );
    if selected == Backend::Exactextract {
        ensure!(
            cfg!(feature = "exactextract"),
            "exactextract backend is not installed; build Skarve with the optional exactextract feature or select native"
        );
        ensure!(
            !prepared,
            "exactextract backend cannot consume a native prepared index"
        );
    } else {
        ensure!(
            v.get("backend_options").is_none() || requested == "auto",
            "backend_options apply to the optional exactextract candidate, not forced native execution"
        );
    }
    options.rasterio_compatible = policy == EXACTEXTRACT_RASTERIO_POLICY;
    let interpretation = if options.rasterio_compatible {
        "rasterio_unscaled_raw_v030"
    } else {
        "skarve_normalized_f64_v1"
    };
    let reason = match (requested, selected) {
        ("auto", Backend::Native) => {
            "native preserves the accepted strict policy; no cross-policy cost advantage is assumed"
        }
        ("auto", Backend::Exactextract) => {
            "exactextract satisfies an explicitly accepted optional numerical policy"
        }
        (_, Backend::Native) => "native strict default or explicit selection",
        _ if options.rasterio_compatible => {
            "explicit pinned exactextract with the unscaled Rasterio input contract"
        }
        _ => "explicit pinned exactextract fractional calculation over Skarve-interpreted values",
    };
    let provenance = json!({"requested_backend":requested,
        "selected_backend":if selected == Backend::Native {"native"} else {"exactextract"},
        "numerical_policy":policy,"accepted_policies":accepted,
        "upstream_version":if selected == Backend::Exactextract {Some("0.3.0")} else {None},
        "source_interpretation":interpretation,
        "coverage_grid_interpretation":if options.rasterio_compatible {"rasterio_bounds_resolution_v030"} else {"native_affine_resolution"},
        "execution_envelope":envelope,"selection_reason":reason,
        "hard_process_memory_limit":false,"hard_cancellation":false,
        "cross_policy_bitwise_equivalence":false});
    Ok(Decision {
        selected,
        provenance,
        options,
    })
}

pub fn strip_fields(v: &mut Value) {
    if let Some(o) = v.as_object_mut() {
        for field in REQUEST_FIELDS {
            o.remove(field);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_defaults_and_eligibility() {
        assert_eq!(
            resolve(&json!({}), false).unwrap().selected,
            Backend::Native
        );
        assert_eq!(
            resolve(
                &json!({"backend":"auto","accepted_policies":[EXACTEXTRACT_POLICY,NATIVE_POLICY]}),
                false
            )
            .unwrap()
            .selected,
            Backend::Native
        );
        assert!(
            resolve(
                &json!({"backend":"exactextract","numerical_policy":NATIVE_POLICY}),
                false
            )
            .is_err()
        );
        assert!(resolve(&json!({"execution_envelope":"process_isolated"}), false).is_err());
        assert!(resolve(&json!({"backend":"auto","accepted_policies":[]}), false).is_err());
        assert!(
            resolve(
                &json!({"backend":"native","backend_options":{"unknown":1}}),
                false
            )
            .is_err()
        );
        assert!(resolve(&json!({"backend":"exactextract"}), true).is_err());
    }
    #[test]
    fn rasterio_policy_is_explicit_and_cannot_be_an_option_override() {
        assert!(
            !resolve(&json!({}), false)
                .unwrap()
                .options
                .rasterio_compatible
        );
        assert!(
            resolve(
                &json!({"backend":"exactextract","backend_options":{"rasterio_compatible":true}}),
                false
            )
            .is_err()
        );
        assert!(
            resolve(
                &json!({"backend":"native","numerical_policy":EXACTEXTRACT_RASTERIO_POLICY}),
                false
            )
            .is_err()
        );
        assert!(
            resolve(
                &json!({"backend":"exactextract","numerical_policy":EXACTEXTRACT_RASTERIO_POLICY}),
                true
            )
            .is_err()
        );
        let result = resolve(
            &json!({"backend":"exactextract","numerical_policy":EXACTEXTRACT_RASTERIO_POLICY}),
            false,
        );
        if cfg!(feature = "exactextract") {
            let decision = result.unwrap();
            assert!(decision.options.rasterio_compatible);
            assert_eq!(
                decision.provenance["numerical_policy"],
                EXACTEXTRACT_RASTERIO_POLICY
            );
            assert_eq!(
                decision.provenance["source_interpretation"],
                "rasterio_unscaled_raw_v030"
            );
            assert!(
                serde_json::to_value(&decision.options)
                    .unwrap()
                    .get("rasterio_compatible")
                    .is_none()
            );
            let legacy = resolve(&json!({"backend":"exactextract"}), false).unwrap();
            assert_eq!(legacy.provenance["numerical_policy"], EXACTEXTRACT_POLICY);
            assert!(!legacy.options.rasterio_compatible);
            let multiple = resolve(&json!({"backend":"exactextract","accepted_policies":[EXACTEXTRACT_RASTERIO_POLICY,EXACTEXTRACT_POLICY]}), false).unwrap();
            assert_eq!(multiple.provenance["numerical_policy"], EXACTEXTRACT_POLICY);
        } else {
            assert!(result.is_err());
        }
    }
}
