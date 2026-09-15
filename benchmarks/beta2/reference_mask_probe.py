#!/usr/bin/env python3
"""Finite pinned-upstream validity representation probe; no Skarve execution."""
import argparse
import importlib.metadata
from pathlib import Path
import numpy as np
from exactextract.raster import NumPyRasterSource
from common import CRS, STRATEGIES, array_rasters, differences, digest, fractional_oracle, rectangle, upstream, write_json


def main():
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--output',required=True)
    args=parser.parse_args();assert importlib.metadata.version('exactextract')=='0.3.0'
    import exactextract.raster
    module=Path(exactextract.raster.__file__)
    values=np.arange(80,dtype=np.float64).reshape(8,10);values[3,4]=-9999.
    valid=np.ones_like(values,dtype=np.uint8);valid[3,4]=0
    normalized={'values':[values],'valid':[valid],'extent':[0.,0.,10.,8.],
                'transform':[0.,1.,0.,8.,0.,-1.],'crs':CRS}
    zone={'id':'crop','geometry':rectangle(2.25,1.5,7.75,6.25)}
    expected=fractional_oracle(values,valid,normalized['transform'],[(1,[2.25,1.5,7.75,6.25])])
    rows=[]
    for encoding in ('masked_array','invalid_nan'):
        for strategy in STRATEGIES:
            rasters=(array_rasters(normalized) if encoding=='invalid_nan' else
                [NumPyRasterSource(np.ma.array(values,mask=~valid.astype(bool)),0,0,10,8,name='a',srs_wkt=CRS)])
            actual=upstream(rasters,[zone],CRS,strategy)[0]['bands']
            rows.append({'encoding':encoding,'strategy':strategy,'answer':actual,
                         'differences':differences(actual,[expected],strict=False)})
    write_json(args.output,{'scope':'Independent pinned-upstream 8x10 cropped validity probe, no engine or scored timings. '
        'Invalid-only NaN encoding is an oracle representation correction, not an engine performance improvement.',
        'upstream_version':'0.3.0','source_pointer':'https://github.com/isciences/exactextract/blob/v0.3.0/python/src/exactextract/raster.py',
        'raster_module_sha256':digest(module),'extension_sha256':{p.name:digest(p) for p in module.parent.glob('_exactextract*.so')},
        'input_values':values.tolist(),'validity':valid.tolist(),'zone':zone,'exact_fraction_reference':expected,
        'rows':rows,'compatible_encoding_passed':all(not row['differences'] for row in rows if row['encoding']=='invalid_nan')})


if __name__=='__main__':main()
