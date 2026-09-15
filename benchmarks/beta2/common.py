"""Shared beta2 evidence shape and independent normalized-value controls.

No measured source is decoded by Skarve callers here: normalized arrays are
restricted to the independent control/calibration lane and charged explicitly.
"""
from __future__ import annotations

from collections import Counter
from fractions import Fraction
import gzip
import hashlib
import json
import math
from pathlib import Path

FIELDS = ('sum', 'support', 'mean', 'min', 'max')
NATIVE_FIELDS = ('fractional_sum', 'covered_cell_equivalents',
                 'coverage_weighted_mean', 'min', 'max')
NATIVE_POLICY = 'native_grid_planar_fractional'
EE_POLICY = 'exactextract_fractional_v030'
INTERPRETATION = 'skarve_normalized_f64_v1'
STRATEGIES = ('feature-sequential', 'raster-sequential')
CRS = 'EPSG:3857'


def digest(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def read_json(path):
    with (gzip.open(path, 'rt') if str(path).endswith('.gz') else open(path)) as stream:
        return json.load(stream)


def write_json(path, value):
    path = Path(path)
    if path.exists():
        raise FileExistsError('Use a fresh evidence destination')
    path.parent.mkdir(parents=True, exist_ok=True)
    with (gzip.open(path, 'wt') if str(path).endswith('.gz') else path.open('w')) as stream:
        json.dump(value, stream, indent=2, allow_nan=False)
        stream.write('\n')


def rectangle(a, b, c, d):
    return {'type':'Polygon', 'coordinates':[[[a,b],[c,b],[c,d],[a,d],[a,b]]]}


def parts_geometry(parts):
    if len(parts) == 1:
        return rectangle(*parts[0][1])
    if len(parts) == 2 and parts[1][0] == -1:
        geometry = rectangle(*parts[0][1])
        geometry['coordinates'].append(rectangle(*parts[1][1])['coordinates'][0][::-1])
        return geometry
    assert all(sign == 1 for sign, _ in parts)
    return {'type':'MultiPolygon', 'coordinates':[rectangle(*bounds)['coordinates'] for _,bounds in parts]}


def fractional_oracle(values, valid, transform, parts):
    """Exact Fraction coverage on the supplied binary64 affine and rectangles.

    The values themselves are already interpreted binary64. No decimal-world
    grid or rounded world-cell denominator replaces the declared affine.
    """
    height, width = values.shape
    origin_x, dx, shear_x, origin_y, shear_y, dy = map(Fraction, transform)
    assert shear_x == shear_y == 0 and dx > 0 and dy < 0
    cell_area = dx * -dy
    total = support = mass = Fraction(0)
    contributors = []
    for row in range(height):
        top, bottom = origin_y + row*dy, origin_y + (row+1)*dy
        for col in range(width):
            left, right = origin_x + col*dx, origin_x + (col+1)*dx
            fraction = Fraction(0)
            for sign, bounds in parts:
                a,b,c,d = map(Fraction, bounds)
                fraction += sign*max(Fraction(0),min(c,right)-max(a,left))*max(Fraction(0),min(d,top)-max(b,bottom))/cell_area
            assert 0 <= fraction <= 1
            if fraction and valid[row,col]:
                value = Fraction(float(values[row,col]))
                total += value*fraction
                mass += abs(value*fraction)
                support += fraction
                contributors.append(float(value))
    return {'sum':float(total), 'support':float(support),
            'mean':float(total/support) if support else None,
            'min':min(contributors) if contributors else None,
            'max':max(contributors) if contributors else None, 'mass':float(mass)}


def normalized_file(path, bands=None):
    """Independent Rasterio decode, then raw-mask/NoData and f64 scale/offset.

    This deliberately does not use exactextract's Float32 Rasterio arithmetic.
    It provides the same interpreted data contract as Skarve's reader.
    """
    import numpy as np
    import rasterio
    with rasterio.open(path) as source:
        bands = list(range(source.count)) if bands is None else list(bands)
        arrays, masks = [], []
        for band in bands:
            raw = source.read(band+1)
            valid = (source.read_masks(band+1) != 0) & np.isfinite(raw)
            nodata = source.nodatavals[band]
            if nodata is not None:
                valid &= ~np.isnan(raw) if math.isnan(nodata) else raw != nodata
            values = raw.astype('float64')*source.scales[band] + source.offsets[band]
            valid &= np.isfinite(values)
            arrays.append(np.ascontiguousarray(values))
            masks.append(np.ascontiguousarray(valid, dtype='u1'))
        return {'values':arrays, 'valid':masks, 'extent':list(source.bounds),
                'transform':list(source.transform.to_gdal()), 'crs':source.crs.to_wkt(),
                'bands':bands, 'decoded_bytes':sum(x.nbytes for x in arrays+masks)}


def upstream(rasters, zones, crs, strategy, max_cells=1_000_000):
    """One natural public all-feature/all-band call, complete canonical output."""
    import exactextract
    from exactextract.feature import JSONFeatureSource
    from exactextract.operation import Operation
    operations = [Operation('count' if field=='support' else field, f'b{band}_{field}', raster)
                  for band,raster in enumerate(rasters) for field in FIELDS]
    features = JSONFeatureSource([{'type':'Feature', 'properties':{'zone_id':z['id']},
                                  'geometry':z['geometry']} for z in zones], srs_wkt=crs)
    raw = exactextract.exact_extract(rasters, features, operations,
            include_cols=['zone_id'], strategy=strategy, max_cells_in_memory=max_cells)
    wanted = [z['id'] for z in zones]
    assert len(set(wanted)) == len(wanted)
    seen = Counter(row['properties']['zone_id'] for row in raw)
    assert seen == Counter(wanted), 'Missing, duplicate or unknown upstream feature'
    output = {}
    for row in raw:
        props = row['properties']
        expected_fields = {'zone_id'} | {f'b{band}_{field}' for band in range(len(rasters)) for field in FIELDS}
        assert set(props) == expected_fields, 'Missing or extra upstream band/statistic'
        bands = []
        for band in range(len(rasters)):
            value = {field:props[f'b{band}_{field}'] for field in FIELDS}
            for field in FIELDS:
                x = value[field]
                if value['support']==0 and field in {'mean','min','max'} and (x is None or not math.isfinite(x)):
                    value[field] = None
                else:
                    assert x is not None and math.isfinite(x), 'Nonempty nonfinite upstream output'
                    value[field] = float(x)
            bands.append(value)
        output[props['zone_id']] = bands
    return [{'zone':key, 'bands':output[key]} for key in wanted]


def array_rasters(normalized, prefix='array'):
    import numpy as np
    from exactextract.raster import NumPyRasterSource
    rasters=[]
    for i,(values,valid) in enumerate(zip(normalized['values'],normalized['valid'])):
        # Pinned NumPyRasterSource's masked-array raster traversal can lose the
        # mask on cropped windows. Encode only invalid cells as explicit NaN;
        # all valid binary64 payloads remain unchanged. This copied snapshot is
        # an unscored independent oracle, never installed-source timed work.
        interpreted=values.copy();mask=valid.astype(bool)
        interpreted[~mask]=np.nan
        assert np.array_equal(interpreted[mask].view(np.uint64),values[mask].view(np.uint64))
        rasters.append(NumPyRasterSource(interpreted,*normalized['extent'],nodata=np.nan,
            name=f'{prefix}{i}',srs_wkt=normalized['crs']))
    return rasters


def native_bands(result, expected_ids=None):
    rows = result['bands']
    if expected_ids is not None:
        assert [row['band'] for row in rows] == list(expected_ids), 'Missing, duplicate or reordered native band IDs'
    return [{field:row[native] for field,native in zip(FIELDS,NATIVE_FIELDS)} for row in rows]


def differences(actual, expected, *, strict):
    assert len(actual) == len(expected), 'Missing or extra bands'
    errors = []
    for band,(a,e) in enumerate(zip(actual,expected)):
        assert set(a) == set(FIELDS), 'Incomplete common output shape'
        for field in FIELDS:
            x,y = a[field],e[field]
            if x is None or y is None:
                if x != y: errors.append({'band':band,'field':field,'actual':x,'expected':y,'reason':'defined/null mismatch'})
                continue
            assert math.isfinite(x) and math.isfinite(y), 'Nonfinite canonical value'
            error = abs(x-y)
            tolerance = (1e-8+1e-10*(e.get('mass',abs(y)) if field=='sum' else abs(y))) if strict else 0
            if error > tolerance:
                errors.append({'band':band,'field':field,'actual':x,'expected':y,
                               'absolute_error':error,'tolerance':tolerance})
    return errors
