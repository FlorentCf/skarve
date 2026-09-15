#!/usr/bin/env python3
"""Tiny upstream source-identity diagnosis; no historical adapter is imported."""
import argparse
import importlib.metadata
from pathlib import Path

import numpy as np
import exactextract.raster
from exactextract.raster import NumPyRasterSource, RasterSource
from common import digest, rectangle, upstream, write_json


class View(RasterSource):
    """Minimal independently authored forwarding source for the identity probe."""
    def __init__(self, source, name):
        super().__init__()
        self.source = source
        if name:
            self.set_name(name)

    def extent(self): return self.source.extent()
    def res(self): return self.source.res()
    def srs_wkt(self): return self.source.srs_wkt()
    def nodata_value(self): return self.source.nodata_value()
    def read_window(self, *args): return self.source.read_window(*args)


def probe():
    assert importlib.metadata.version('exactextract') == '0.3.0'
    rows = []
    for named in (False, True):
        for strategy in ('feature-sequential', 'raster-sequential'):
            rasters = [View(NumPyRasterSource(np.array([[1., 2.], [3., 4.]]) * scale,
                                             name=f'base{i}'), f'wrapped{i}' if named else None)
                       for i, scale in enumerate((1, 10))]
            result = upstream(rasters, [{'id': 'zone', 'geometry': rectangle(0, 0, 2, 2)}],
                              None, strategy)[0]['bands']
            expected = [{'sum': 10., 'support': 4., 'mean': 2.5, 'min': 1., 'max': 4.},
                        {'sum': 100., 'support': 4., 'mean': 25., 'min': 10., 'max': 40.}]
            rows.append({'unique_wrapper_names': named, 'strategy': strategy,
                         'actual': result, 'expected': expected, 'matches': result == expected})
    passed = all(row['matches'] == row['unique_wrapper_names'] for row in rows)
    return {'schema': 'skarve_upstream_source_name_probe_v1', 'diagnosis_reproduced': passed,
            'upstream_version': importlib.metadata.version('exactextract'),
            'upstream_raster_module_sha256': digest(exactextract.raster.__file__),
            'harness_sha256': digest(__file__), 'rows': rows,
            'scope': 'Untimed 2x2, two-band forwarding-source probe. No historical adapter, engine, file source, mask, or transport modification. Unique wrapper names are the sole changed input.'}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    assert not args.output.exists()
    result = probe()
    write_json(args.output, result)
    print({'diagnosis_reproduced': result['diagnosis_reproduced'], 'rows': len(result['rows'])})
    raise SystemExit(0 if result['diagnosis_reproduced'] else 1)
