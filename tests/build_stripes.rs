use anyhow::{Result, ensure};
use raster_engine::{
    model::{Band, Grid, Raster},
    persistent::{self, BoundarySource},
    source::{BandMetadata, RasterMetadata, ReadMetrics, WindowSource},
};
use std::{cell::RefCell, sync::atomic::AtomicBool};

struct Source {
    meta: RasterMetadata,
    reads: RefCell<Vec<[usize; 4]>>,
    reject_stripe_budget: bool,
}
impl Source {
    fn new(striped: bool, reject_stripe_budget: bool) -> Self {
        Self {
            meta: RasterMetadata {
                grid: Grid {
                    width: 97,
                    height: 35,
                    transform: [0., 1., 0., 35., 0., -1.],
                    crs: "LOCAL".into(),
                },
                source_id: "deterministic-build-fixture".into(),
                bands: (0..3)
                    .map(|_| BandMetadata {
                        data_type: "Float64".into(),
                        nodata: None,
                        scale: 1.,
                        offset: 0.,
                        unit: None,
                        block_size: if striped { (97, 1) } else { (16, 16) },
                    })
                    .collect(),
            },
            reads: RefCell::new(Vec::new()),
            reject_stripe_budget,
        }
    }
}
impl WindowSource for Source {
    fn metadata(&self) -> &RasterMetadata {
        &self.meta
    }
    fn verify_immutable(&self) -> Result<()> {
        Ok(())
    }
    fn read_buffer_bound(&self, w: usize, h: usize, b: &[usize]) -> Result<usize> {
        Ok(if self.reject_stripe_budget && w == 97 {
            usize::MAX
        } else {
            w * h * b.len() * 9
        })
    }
    fn read_selected_window_cancellable(
        &self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        bands: &[usize],
        budget: usize,
        _: &AtomicBool,
    ) -> Result<(Raster, ReadMetrics)> {
        ensure!(
            w * h * bands.len() * 9 <= budget,
            "read exceeded its declared budget"
        );
        self.reads.borrow_mut().push([x, y, w, h]);
        let mut grid = self.meta.grid.clone();
        grid.width = w;
        grid.height = h;
        grid.transform[0] = x as f64;
        grid.transform[3] = 35. - y as f64;
        let bands = bands
            .iter()
            .map(|b| {
                let mut values = Vec::new();
                let mut valid = Vec::new();
                for yy in y..y + h {
                    for xx in x..x + w {
                        values.push(((xx * 3 + yy * 7 + b * 5) % 41) as f64 - 20.);
                        valid.push((xx + yy * 3 + b) % 11 != 0);
                    }
                }
                Band {
                    values,
                    valid,
                    unit: None,
                }
            })
            .collect();
        Ok((
            Raster {
                grid,
                bands,
                source_id: self.meta.source_id.clone(),
            },
            ReadMetrics {
                raster_io_calls: 1,
                ..Default::default()
            },
        ))
    }
}

#[test]
fn stripe_build_reads_once_and_preserves_every_summary_and_normalized_value_bit() {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("skarve-stripe-{}-{nonce}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    for layout in ["band_major", "cell_major", "band_groups_4"] {
        let reference = Source::new(false, false);
        let stripe = Source::new(true, false);
        let fallback = Source::new(true, true);
        let dirs = [
            root.join(format!("{layout}-tiles")),
            root.join(format!("{layout}-stripe")),
            root.join(format!("{layout}-fallback")),
        ];
        for (i, source) in [&reference, &stripe, &fallback].iter().enumerate() {
            let result = persistent::build_from_source(
                *source,
                dirs[i].to_str().unwrap(),
                16,
                layout,
                "hierarchy",
                BoundarySource::Normalized,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(result["builder_windows_read"], if i == 1 { 3 } else { 21 });
            assert_eq!(
                result["builder_read_policy"],
                if i == 1 {
                    "source_stripes"
                } else {
                    "summary_tiles"
                }
            );
            assert!(result["builder_buffer_bound_bytes"].as_u64().unwrap() <= 256 << 20);
        }
        assert_eq!(stripe.reads.borrow().len(), 3);
        assert!(
            stripe
                .reads
                .borrow()
                .iter()
                .all(|r| r[0] == 0 && r[2] == 97)
        );
        for filename in ["summary.rsi", "pixels.rsr"] {
            let expected = std::fs::read(dirs[0].join(filename)).unwrap();
            for path in &dirs[1..] {
                let actual = std::fs::read(path.join(filename)).unwrap();
                // Header identities intentionally include different physical
                // metadata/build versions; all actual records must be identical.
                assert_eq!(&actual[65536..], &expected[65536..], "{layout}/{filename}");
            }
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}
