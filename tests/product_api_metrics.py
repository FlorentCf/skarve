#!/usr/bin/env python3
"""Installed selected-statistic contract; fixture generation is a separate process."""
import argparse
import json
from pathlib import Path


def generate(folder):
    import numpy as np
    import rasterio
    from rasterio.transform import from_origin

    folder.mkdir(parents=True, exist_ok=False)
    with rasterio.open(folder / 'large-finite.tif', 'w', driver='GTiff', width=2,
                       height=2, count=1, dtype='float64', crs='EPSG:3857',
                       transform=from_origin(0, 2, 1, 1)) as dataset:
        dataset.write(np.full((1, 2, 2), 1e308, dtype='float64'))
    zone = {'type':'Polygon', 'coordinates':[[[0,0],[2,0],[2,2],[0,2],[0,0]]]}
    fixture = {'source':'large-finite.tif', 'crs':'EPSG:3857', 'zone':zone, 'min':1e308}
    (folder / 'fixture.json').write_text(json.dumps(fixture, allow_nan=False) + '\n')
    (folder / 'zone.json').write_text(json.dumps(zone) + '\n')
    return {'generated':True, 'rows':2, 'columns':2, 'bands':1, 'external_requests':0}


def check(path, backend):
    from skarve import Skarve, EngineError

    fixture = json.loads(path.read_text())
    source_path = path.parent / fixture['source']
    def minimum(band):
        assert band['min'] == fixture['min']
        assert 'fractional_sum' not in band and 'coverage_weighted_mean' not in band
    with Skarve() as sk:
        with sk.infuse(source_path) as source:
            result = source.carve(zone=fixture['zone'], metrics=['min'], backend=backend)
            minimum(result['bands'][0])
            if backend == 'exactextract':
                assert result['work']['bridge_abi_version'] == 2
                assert result['work']['statistics_mask'] == 8
            result = source.carve(zone=fixture['zone'], metrics=['max','support'], backend=backend)
            assert result['bands'][0]['max'] == fixture['min']
            assert result['bands'][0]['covered_cell_equivalents'] == 4
            assert 'fractional_sum' not in result['bands'][0]
            try:
                source.carve(zone=fixture['zone'], metrics=['sum'], backend=backend)
            except EngineError as error:
                assert 'overflow' in str(error).lower() or 'nonfinite' in str(error).lower()
            else:
                raise AssertionError('Requested overflowing sum was accepted')
            minimum(source.carve(zone=fixture['zone'], metrics=['min'], backend=backend)['bands'][0])
        job = {'zones':[{'id':name,'version':'1','geometry':fixture['zone']} for name in ('a','b')],
               'slices':[{'id':'slice','spec':{'location':str(source_path)},'bands':[0]}],
               'crs':fixture['crs'],'metrics':['min'],'backend':backend}
        rows = 0
        for page in sk.cleave(job, max_rows=1):
            assert len(page['rows']) <= 1
            for row in page['rows']:
                minimum(row['bands'][0]); rows += 1
        assert rows == 2 and page['complete']
    return {'passed':True,'backend':backend,'single_min':1e308,'batch_rows':rows,
            'requested_sum_overflow_rejected':True,'unrequested_sum_not_required':True}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=['generate','check'])
    parser.add_argument('path', type=Path)
    parser.add_argument('--backend', choices=['native','exactextract'], default='native')
    args = parser.parse_args()
    result = generate(args.path) if args.mode == 'generate' else check(args.path, args.backend)
    print(json.dumps(result, allow_nan=False))
