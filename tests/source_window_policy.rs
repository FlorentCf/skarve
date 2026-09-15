use anyhow::Result;
use raster_engine::{
    model::{Grid, Raster},
    source::{BandMetadata, RasterMetadata, ReadMetrics, WindowSource, select_window_edge},
};
use std::sync::atomic::AtomicBool;
struct Hint(RasterMetadata);
impl WindowSource for Hint {
    fn metadata(&self) -> &RasterMetadata {
        &self.0
    }
    fn verify_immutable(&self) -> Result<()> {
        Ok(())
    }
    fn read_selected_window_cancellable(
        &self,
        _: usize,
        _: usize,
        _: usize,
        _: usize,
        _: &[usize],
        _: usize,
        _: &AtomicBool,
    ) -> Result<(Raster, ReadMetrics)> {
        panic!("policy must not read values")
    }
}
fn source(block: (usize, usize)) -> Hint {
    Hint(RasterMetadata {
        grid: Grid {
            width: 1024,
            height: 1024,
            transform: [0., 1., 0., 1024., 0., -1.],
            crs: "LOCAL".into(),
        },
        bands: vec![BandMetadata {
            data_type: "Float64".into(),
            nodata: None,
            scale: 1.,
            offset: 0.,
            unit: None,
            block_size: block,
        }],
        source_id: "policy-only".into(),
    })
}
#[test]
fn physical_window_policy_coalesces_dense_demand_but_rejects_sparse_overread() {
    let source = source((512, 512));
    let dense = (0..8)
        .flat_map(|y| (0..8).map(move |x| (y, x)))
        .collect::<Vec<_>>();
    let result = select_window_edge(&source, 64, &[0], &dense, 8 << 20).unwrap();
    assert_eq!(result.selected_edge, 512);
    assert_eq!(result.candidates[0].decoded_cells, 262144);
    assert_eq!(result.candidates.last().unwrap().decoded_cells, 262144);
    assert_eq!(result.candidates.last().unwrap().data_block_incidents, 1);
    let sparse = select_window_edge(&source, 64, &[0], &[(0, 0), (15, 15)], 8 << 20).unwrap();
    assert_eq!(sparse.selected_edge, 64);
    assert!(
        sparse
            .candidates
            .iter()
            .skip(1)
            .all(|c| c.reason == "sparse_logical_overread")
    );
}
#[test]
fn window_policy_respects_memory_and_non_square_storage_and_input_bounds() {
    let source = source((512, 512));
    let dense = (0..8)
        .flat_map(|y| (0..8).map(move |x| (y, x)))
        .collect::<Vec<_>>();
    let limited = select_window_edge(&source, 64, &[0], &dense, 128 * 128 * 9).unwrap();
    assert_eq!(limited.selected_edge, 128);
    assert_eq!(
        select_window_edge(&self::source((1024, 8)), 64, &[0], &dense, 8 << 20)
            .unwrap()
            .selected_edge,
        64
    );
    assert!(select_window_edge(&source, 64, &[0], &[(16, 0)], 8 << 20).is_err());
    assert!(select_window_edge(&source, 64, &[1], &dense, 8 << 20).is_err());
}

#[test]
fn optional_larger_window_refusal_falls_back_to_requested_edge() {
    struct Restricted(Hint);
    impl WindowSource for Restricted {
        fn metadata(&self) -> &RasterMetadata {
            self.0.metadata()
        }
        fn verify_immutable(&self) -> Result<()> {
            Ok(())
        }
        fn read_buffer_bound(&self, w: usize, h: usize, indices: &[usize]) -> Result<usize> {
            anyhow::ensure!(w <= 64 && h <= 64, "reader supports only small windows");
            Ok(w * h * indices.len() * 9)
        }
        fn read_selected_window_cancellable(
            &self,
            _: usize,
            _: usize,
            _: usize,
            _: usize,
            _: &[usize],
            _: usize,
            _: &AtomicBool,
        ) -> Result<(Raster, ReadMetrics)> {
            panic!("no reads")
        }
    }
    let dense = (0..8)
        .flat_map(|y| (0..8).map(move |x| (y, x)))
        .collect::<Vec<_>>();
    let result =
        select_window_edge(&Restricted(source((512, 512))), 64, &[0], &dense, 8 << 20).unwrap();
    assert_eq!(result.selected_edge, 64);
    assert!(
        result
            .candidates
            .iter()
            .skip(1)
            .all(|c| c.reason == "optional_read_bound_refused")
    );
}

struct WideHint(Hint, usize);
impl WindowSource for WideHint {
    fn metadata(&self) -> &RasterMetadata {
        self.0.metadata()
    }
    fn verify_immutable(&self) -> Result<()> {
        Ok(())
    }
    fn max_read_bands(&self) -> usize {
        self.1
    }
    fn read_selected_window_cancellable(
        &self,
        _: usize,
        _: usize,
        _: usize,
        _: usize,
        _: &[usize],
        _: usize,
        _: &AtomicBool,
    ) -> Result<(Raster, ReadMetrics)> {
        panic!("planner must not read values")
    }
}

#[test]
fn planner_respects_each_readers_band_capability_and_absolute_64_bound() {
    let mut ordinary = source((256, 256));
    ordinary.0.bands = vec![ordinary.0.bands[0].clone(); 65];
    let dense = (0..4)
        .flat_map(|y| (0..4).map(move |x| (y, x)))
        .collect::<Vec<_>>();
    assert_eq!(ordinary.max_read_bands(), 20);
    assert_eq!(
        select_window_edge(
            &ordinary,
            128,
            &(0..20).collect::<Vec<_>>(),
            &dense,
            64 << 20
        )
        .unwrap()
        .selected_edge,
        256
    );
    assert!(
        select_window_edge(
            &ordinary,
            128,
            &(0..21).collect::<Vec<_>>(),
            &dense,
            64 << 20
        )
        .is_err()
    );
    for cap in [40, 64] {
        let reader = WideHint(source((256, 256)), cap);
        let mut reader = reader;
        reader.0.0.bands = vec![reader.0.0.bands[0].clone(); 65];
        let selected = (0..cap).rev().collect::<Vec<_>>();
        let decision = select_window_edge(&reader, 128, &selected, &dense, 64 << 20).unwrap();
        assert_eq!(decision.selected_edge, 256);
        assert!(
            decision.candidates[1].data_block_incidents
                < decision.candidates[0].data_block_incidents
        );
        assert!(
            select_window_edge(
                &reader,
                128,
                &(0..cap + 1).collect::<Vec<_>>(),
                &dense,
                64 << 20
            )
            .is_err()
        );
        let budget = reader.read_buffer_bound(128, 128, &selected).unwrap();
        let limited = select_window_edge(&reader, 128, &selected, &dense, budget).unwrap();
        assert_eq!(limited.selected_edge, 128);
        assert_eq!(limited.candidates[1].reason, "read_buffer_budget");
        let sparse = select_window_edge(&reader, 128, &selected, &[(0, 0)], 64 << 20).unwrap();
        assert_eq!(sparse.selected_edge, 128);
        assert_eq!(sparse.candidates[1].reason, "sparse_logical_overread");
    }
    let overstated = WideHint(ordinary, 128);
    assert!(
        select_window_edge(
            &overstated,
            128,
            &(0..65).collect::<Vec<_>>(),
            &dense,
            128 << 20
        )
        .is_err()
    );
}
