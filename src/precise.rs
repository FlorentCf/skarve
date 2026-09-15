//! Bounded exact-rational coverage for ill-conditioned, very small support.
//! Interpret submitted binary64 world coordinates and affine coefficients
//! exactly. Only final area fractions are rounded back to binary64.
use crate::{
    coverage::Cell,
    model::{Grid, Sum, check_cancel},
};
use anyhow::{Result, ensure};
use num_rational::BigRational as R;
use num_traits::{Signed, ToPrimitive, Zero};
use serde_json::Value;
use std::sync::atomic::AtomicBool;

#[derive(Clone)]
struct Point {
    x: R,
    y: R,
}
fn f(v: f64) -> R {
    R::from_float(v).expect("validated finite coordinate")
}
fn integer(v: usize) -> R {
    R::from_integer(v.into())
}
fn clip(
    input: &[Point],
    axis: usize,
    value: usize,
    greater: bool,
    cancel: &AtomicBool,
) -> Result<Vec<Point>> {
    check_cancel(cancel)?;
    if input.is_empty() {
        return Ok(Vec::new());
    }
    let bound = integer(value);
    let coord = |p: &Point| if axis == 0 { p.x.clone() } else { p.y.clone() };
    let inside = |p: &Point| {
        if greater {
            coord(p) >= bound
        } else {
            coord(p) <= bound
        }
    };
    let mut output = Vec::new();
    let mut a = input.last().unwrap();
    for (index, b) in input.iter().enumerate() {
        if index % 64 == 0 {
            check_cancel(cancel)?;
        }
        if inside(a) != inside(b) {
            let t = (&bound - coord(a)) / (coord(b) - coord(a));
            let mut p = Point {
                x: &a.x + &t * (&b.x - &a.x),
                y: &a.y + &t * (&b.y - &a.y),
            };
            if axis == 0 {
                p.x = bound.clone();
            } else {
                p.y = bound.clone();
            }
            output.push(p);
        }
        if inside(b) {
            output.push(b.clone());
        }
        a = b;
    }
    Ok(output)
}
fn area(points: &[Point], cancel: &AtomicBool) -> Result<R> {
    check_cancel(cancel)?;
    if points.len() < 3 {
        return Ok(R::zero());
    }
    let origin = &points[0];
    let mut total = R::zero();
    let mut a = points.last().unwrap();
    for (index, b) in points.iter().enumerate() {
        if index % 64 == 0 {
            check_cancel(cancel)?;
        }
        total += (&a.x - &origin.x) * (&b.y - &origin.y) - (&b.x - &origin.x) * (&a.y - &origin.y);
        a = b;
    }
    Ok(total.abs() / integer(2))
}
pub fn coverage(
    grid: &Grid,
    input: &Value,
    bounds: [usize; 4],
    vertices: usize,
    max_bytes: usize,
    cancel: &AtomicBool,
) -> Result<(Vec<Cell>, f64, f64)> {
    check_cancel(cancel)?;
    let [xmin, xmax, lo, hi] = bounds;
    let cells = (xmax - xmin)
        .checked_mul(hi - lo)
        .ok_or_else(|| anyhow::anyhow!("precision work overflow"))?;
    ensure!(
        cells <= 100_000 && cells.checked_mul(vertices).is_some_and(|n| n <= 1_000_000),
        "exact precision fallback work budget exceeded"
    );
    ensure!(
        vertices
            .saturating_mul(4096)
            .saturating_add(cells.saturating_mul(48))
            <= max_bytes,
        "exact precision fallback memory budget exceeded"
    );
    let mut rings = Vec::new();
    let polygons: Vec<&Value> = if input["type"] == "Polygon" {
        vec![&input["coordinates"]]
    } else {
        input["coordinates"].as_array().unwrap().iter().collect()
    };
    for polygon in polygons {
        for (index, ring) in polygon.as_array().unwrap().iter().enumerate() {
            let coordinates = ring.as_array().unwrap();
            let mut points = Vec::with_capacity(coordinates.len());
            for (coordinate_index, v) in coordinates.iter().enumerate() {
                if coordinate_index % 64 == 0 {
                    check_cancel(cancel)?;
                }
                points.push(Point {
                    x: (f(v[0].as_f64().unwrap()) - f(grid.transform[0])) / f(grid.transform[1]),
                    y: (f(v[1].as_f64().unwrap()) - f(grid.transform[3])) / f(grid.transform[5]),
                });
            }
            rings.push((points, if index == 0 { 1 } else { -1 }));
        }
    }
    let mut polygon_area = R::zero();
    for (ring, sign) in &rings {
        if *sign > 0 {
            polygon_area += area(ring, cancel)?
        } else {
            polygon_area -= area(ring, cancel)?
        }
    }
    let mut selected = Sum::default();
    let mut output = Vec::new();
    for row in lo..hi {
        check_cancel(cancel)?;
        let strips = rings
            .iter()
            .map(|(ring, sign)| {
                Ok((
                    clip(
                        &clip(ring, 1, row, true, cancel)?,
                        1,
                        row + 1,
                        false,
                        cancel,
                    )?,
                    *sign,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        for col in xmin..xmax {
            if col % 64 == 0 {
                check_cancel(cancel)?;
            }
            let mut fraction = R::zero();
            for (ring, sign) in &strips {
                let value = area(
                    &clip(
                        &clip(ring, 0, col, true, cancel)?,
                        0,
                        col + 1,
                        false,
                        cancel,
                    )?,
                    cancel,
                )?;
                if *sign > 0 {
                    fraction += value
                } else {
                    fraction -= value
                }
            }
            ensure!(
                fraction >= R::zero() && fraction <= integer(1),
                "exact coverage outside [0,1]"
            );
            if !fraction.is_zero() {
                let value = fraction
                    .to_f64()
                    .ok_or_else(|| anyhow::anyhow!("exact fraction cannot convert to binary64"))?;
                ensure!(
                    value > 0.,
                    "positive exact coverage below binary64 precision"
                );
                selected.add(value);
                output.push(Cell {
                    row,
                    col,
                    fraction: value,
                });
            }
        }
    }
    Ok((
        output,
        selected.value(),
        polygon_area
            .to_f64()
            .ok_or_else(|| anyhow::anyhow!("exact polygon area cannot convert to binary64"))?,
    ))
}
