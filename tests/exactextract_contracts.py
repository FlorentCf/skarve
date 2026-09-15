#!/usr/bin/env python3
"""Independent installed-interface calibration for an optional exactextract build.

The normal mode imports the installed package. --checkout-bindings is an
explicit development-only alternative; no private repository is required.
"""
from __future__ import annotations
import argparse
import asyncio
import importlib.metadata
import json
from pathlib import Path
import shutil
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT/'benchmarks'/'beta2'))
from common import (CRS, EE_POLICY, FIELDS, INTERPRETATION, NATIVE_POLICY,
                    STRATEGIES, array_rasters, differences, digest,
                    fractional_oracle, native_bands, normalized_file,
                    parts_geometry, rectangle, upstream, write_json)


def fixtures():
    return [
        {'id':'binary_fraction','values':[[[-7.,5.],[3.,2.]]],'parts':[(1,[.25,.5,1.75,1.5])]},
        {'id':'decimal_fraction','values':[[[11.]]],'parts':[(1,[.1,.2,.7,.9])]},
        {'id':'cancellation','values':[[[2.**53,1.,-2.**53]]],'parts':[(1,[0.,0.,3.,1.])]},
        {'id':'positive_sliver','values':[[[7.]]],'parts':[(1,[0.,0.,2.**-130,1.])]},
        {'id':'different_masks','values':[[[8.,-9999.],[-2.,4.]],[[1.,5.],[-9999.,9.]]],
         'parts':[(1,[0.,0.,2.,2.])]},
        {'id':'scale_offset','dtype':'float32','scales':[.1,-2.], 'offsets':[.25,17.],
         'values':[[[11.,-9999.],[3.,5.]],[[2.,6.],[-9999.,9.]]],
         'parts':[(1,[.125,.25,1.75,1.875])]},
        {'id':'all_masked','values':[[[-9999.,-9999.]]],'parts':[(1,[0.,0.,2.,1.])]},
        {'id':'empty','values':[[[1.]]],'parts':[(1,[2.,2.,3.,3.])]},
        {'id':'partial','values':[[[1.,2.],[3.,4.]]],'parts':[(1,[-.5,.25,1.5,2.5])]},
        {'id':'hole','values':[[[1.,2.,3.],[4.,5.,6.],[7.,8.,9.]]],
         'parts':[(1,[0.,0.,3.,3.]),(-1,[.5,.75,2.5,2.25])]},
        {'id':'multipart','values':[[[-1.,9.,2.]]],
         'parts':[(1,[0.,0.,.5,1.]),(1,[2.25,0.,3.,1.])]},
        {'id':'non_power_two_mask','values':[[[1.,-9999.,-3.,7.,11.,-2.,5.],[8.,3.,-9999.,-4.,2.,9.,1.],[0.,1.,2.,3.,4.,5.,6.]]],
         'parts':[(1,[.125,.375,6.875,2.75])]},
        {'id':'translated_affine','values':[[[3.,7.,2.],[5.,-9999.,1.]]],
         'transform':[1000000000.125,.03125,0.,2000000000.25,0.,-.0625],
         'parts':[(1,[1000000000.140625,2000000000.15625,1000000000.203125,2000000000.234375])]},
    ]


def write_fixture(folder, fixture):
    import numpy as np
    import rasterio
    from affine import Affine
    values = np.asarray(fixture['values'], dtype=fixture.get('dtype','float64'))
    count,height,width = values.shape
    transform = fixture.get('transform',[0.,1.,0.,float(height),0.,-1.])
    path = folder/(fixture['id']+'.tif')
    with rasterio.open(path,'w',driver='GTiff',width=width,height=height,count=count,
            dtype=values.dtype,crs=CRS,transform=Affine.from_gdal(*transform),nodata=-9999.) as ds:
        ds.write(values)
        ds.scales = fixture.get('scales',[1.]*count)
        ds.offsets = fixture.get('offsets',[0.]*count)
    return path


def provenance(result, selected, policy):
    p = result['provenance']
    assert p['selected_backend'] == selected
    assert p['numerical_policy'] == policy
    assert p['execution_envelope'] == 'embedded_cooperative'
    assert p['selection_reason']
    if selected == 'exactextract':
        assert p['upstream_version'] == '0.3.0'
        assert p['source_interpretation'] == INTERPRETATION
    return p


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--library',type=Path)
    parser.add_argument('--scratch',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--enabled',choices=['yes','no'],required=True)
    parser.add_argument('--checkout-bindings',action='store_true')
    args = parser.parse_args()
    assert not args.scratch.exists() and not args.output.exists(), 'Retain previous evidence'
    args.scratch.mkdir(parents=True)
    if args.checkout_bindings:sys.path.insert(0,str(ROOT/'bindings'/'python'))
    from skarve import Skarve
    from raster_engine_lab import resolve_library
    rows, checks, failures = [], [], []
    def checked(name, **evidence): checks.append({'name':name,'passed':True,**evidence})
    def rejected(name, function):
        try:function()
        except Exception as error:
            checked(name, error_type=type(error).__name__, error=str(error).replace(str(args.scratch),'<generated-fixtures>'))
        else:raise AssertionError('Expected explicit rejection: '+name)
    try:
        assert importlib.metadata.version('exactextract') == '0.3.0'
        with Skarve(args.library) as engine:
            for fixture in fixtures():
                path = write_fixture(args.scratch,fixture)
                normalized = normalized_file(path)
                geom = parts_geometry(fixture['parts'])
                expected = [fractional_oracle(v,m,normalized['transform'],fixture['parts'])
                            for v,m in zip(normalized['values'],normalized['valid'])]
                with engine.infuse(path,id=fixture['id']) as source:
                    native = source.carve(zone=geom,metrics=list(FIELDS),backend='native')
                    native_errors = differences(native_bands(native),expected,strict=True)
                    assert not native_errors, ('native strict calibration',fixture['id'],native_errors)
                    provenance(native,'native',NATIVE_POLICY)
                    checked('native-'+fixture['id'],bands=len(expected))
                    if args.enabled == 'no':
                        rejected('unavailable-'+fixture['id'],lambda:source.carve(zone=geom,metrics=list(FIELDS),backend='exactextract'))
                        continue
                    rasters = array_rasters(normalized)
                    for strategy in STRATEGIES:
                        reference = upstream(rasters,[{'id':fixture['id'],'geometry':geom}],normalized['crs'],strategy)[0]['bands']
                        actual = source.carve(zone=geom,metrics=list(FIELDS),backend='exactextract',backend_options={'strategy':strategy})
                        p = provenance(actual,'exactextract',EE_POLICY)
                        bridge_errors = differences(native_bands(actual),reference,strict=False)
                        rows.append({'fixture':fixture['id'],'source_sha256':digest(path),'strategy':strategy,
                                     'input':fixture,'interpreted_values':[v.tolist() for v in normalized['values']],
                                     'validity':[v.tolist() for v in normalized['valid']],
                                     'exact_fraction_reference':expected,'upstream_reference':reference,
                                     'actual':native_bands(actual),'bridge_differences':bridge_errors,
                                     'strict_compatibility_differences':differences(reference,expected,strict=True),
                                     'provenance':p})
                        assert not bridge_errors, ('bridge versus normalized upstream',fixture['id'],strategy,bridge_errors)
                    checked('matching-upstream-'+fixture['id'],strategies=2,bands=len(expected))
                    automatic = source.carve(zone=geom,metrics=list(FIELDS),backend='auto')
                    provenance(automatic,'native',NATIVE_POLICY)
                    assert not differences(native_bands(automatic),expected,strict=True)
                    accepted = source.carve(zone=geom,metrics=list(FIELDS),backend='auto',accepted_policies=[EE_POLICY])
                    provenance(accepted,'exactextract',EE_POLICY)
                    checked('auto-policy-eligibility-'+fixture['id'])
            if args.enabled=='yes':
                path=args.scratch/'different_masks.tif'
                geom=rectangle(.125,.25,1.875,1.75)
                normalized=normalized_file(path,bands=[1,0])
                reference=upstream(array_rasters(normalized),[{'id':'mapping','geometry':geom}],normalized['crs'],'raster-sequential')[0]['bands']
                with engine.infuse({'location':str(path),'bands':[1,0]},id='mapped') as source:
                    actual=source.carve(zone=geom,metrics=list(FIELDS),backend='exactextract')
                    assert not differences(native_bands(actual,[0,1]),reference,strict=False)
                checked('registered-source-band-remapping')
                sources=['different_masks','scale_offset'];geometries=[geom,rectangle(3.,3.,4.,4.)]
                zones=[{'id':f'zone{i}','version':'v1','geometry':g} for i,g in enumerate(geometries)]
                handles=[];rasters=[]
                try:
                    for name in sources:
                        normalized=normalized_file(args.scratch/(name+'.tif'))
                        rasters.extend(array_rasters(normalized,prefix=name))
                        handles.append(engine.infuse(args.scratch/(name+'.tif'),id='batch-'+name))
                    reference=upstream(rasters,zones,normalized['crs'],'raster-sequential')
                    job={'zones':zones,'slices':[{'id':name,'source':'batch-'+name} for name in sources],
                         'crs':CRS,'backend':'exactextract','options':{'statistics':list(FIELDS)}}
                    for mode in ('full','numeric'):
                        job['output_mode']=mode;seen=set();row_count=0;last=None
                        for page in engine.cleave(job,max_rows=1,checkpoint_interval=2**30):
                            provenance(page,'exactextract',EE_POLICY)
                            assert len(page['rows'])<=1
                            assert page['checkpoint_supported'] is False and page['checkpoint'] is None
                            for row in page['rows']:
                                key=(row['zone_id'],row['slice_id']);assert key not in seen;seen.add(key)
                                zi=int(row['zone_id'][4:]);si=sources.index(row['slice_id'])
                                expected=reference[zi]['bands'][si*2:si*2+2]
                                assert not differences(native_bands(row,[0,1]),expected,strict=False)
                                row_count+=1
                            last=page
                        assert last['complete'] and row_count==4
                        assert seen=={(z['id'],s) for z in zones for s in sources}
                        checked('all-source-paged-five-field-'+mode,rows=row_count,band_rows=8)
                finally:
                    for handle in reversed(handles):handle.close()
                # An unrequested overflowing sum must not invalidate finite
                # extrema/support. This is independent of the five-field
                # timing programme and catches accidental all-stat execution.
                extreme=write_fixture(args.scratch,{'id':'selected_extrema',
                    'values':[[[1e308,1e308]]],'parts':[(1,[0.,0.,2.,1.])]})
                geom=rectangle(0.,0.,2.,1.)
                with engine.infuse(extreme,id='extrema') as source:
                    for fields in (['min'],['max'],['support'],['min','max']):
                        for strategy in STRATEGIES:
                            actual=source.carve(zone=geom,metrics=fields,backend='exactextract',
                                backend_options={'strategy':strategy})
                            provenance(actual,'exactextract',EE_POLICY)
                            assert len(actual['bands'])==1
                            row=actual['bands'][0]
                            keys={'support':'covered_cell_equivalents','min':'min','max':'max'}
                            assert {field for field in keys if keys[field] in row}==set(fields)
                            for field in fields:assert row[keys[field]]==(2. if field=='support' else 1e308)
                            checked('selected-'+strategy+'-'+'-'.join(fields))
                    job={'zones':[{'id':'z','version':'v1','geometry':geom}],
                        'slices':[{'id':'s','source':'extrema'}],'crs':CRS,
                        'backend':'exactextract','output_mode':'full','options':{'statistics':['min']}}
                    pages=list(engine.cleave(job,max_rows=1,checkpoint_interval=2**30))
                    assert pages[-1]['complete'] and sum(len(p['rows']) for p in pages)==1
                    assert pages[-1]['rows'][0]['bands'][0]['min']==1e308
                    checked('selected-extrema-batch-with-unrequested-overflow')
            # Deterministic admission and source mutation failures exercise
            # reusable ownership without relying on a race or millisecond target.
            path = args.scratch/'binary_fraction.tif';geom=rectangle(.25,.5,1.75,1.5)
            with engine.infuse(path,id='guards') as source:
                for name,options in [
                    ('strict-conflict',{'backend':'exactextract','numerical_policy':NATIVE_POLICY}),
                    ('hard-isolation',{'backend':'exactextract','execution_envelope':'process_isolated'}),
                    ('extra-count',{'backend':'exactextract','statistics':list(FIELDS)+['count']}),
                    ('empty-acceptance',{'backend':'auto','accepted_policies':[]}),
                    ('window-limit',{'backend':'exactextract','backend_options':{'window_bytes':1}}),
                    ('output-limit',{'backend':'exactextract','backend_options':{'output_bytes':1}}),
                ]:
                    rejected(name,lambda options=options:source.measure(geom,CRS,**options))
                recovered=source.carve(zone=geom,backend='native',metrics=list(FIELDS))
                assert len(recovered['bands'])==1
                checked('reusable-after-rejections')
            changed=args.scratch/'changed.tif';shutil.copyfile(path,changed)
            with engine.infuse(changed,id='changed') as source:
                backend='exactextract' if args.enabled=='yes' else 'native'
                source.carve(zone=geom,backend=backend,metrics=list(FIELDS))
                with changed.open('ab') as stream:stream.write(b'generated-mutated-source')
                rejected('source-mutation',lambda:source.carve(zone=geom,backend=backend,metrics=list(FIELDS)))
        checked('engine-and-source-context-cleanup')
    except Exception as error:
        failures.append({'type':type(error).__name__,'error':str(error).replace(str(args.scratch),'<generated-fixtures>')})
    result={'schema':'skarve_optional_exactextract_calibration_v1','passed':not failures,
            'enabled':args.enabled=='yes','binding_mode':'explicit checkout development' if args.checkout_bindings else 'installed package',
            'library_sha256':digest(resolve_library(args.library)),
            'harness_sha256':digest(__file__),'common_sha256':digest(ROOT/'benchmarks/beta2/common.py'),
            'checks':checks,'rows':rows,'failures':failures,
            'scope':'Exact Fraction affine-rectangle oracle plus independent pinned upstream over identically interpreted f64/masks. No measurements or network sources.',
            'limitations':['No general numerical equivalence claim for upstream versus native strict.',
                           'Embedded cancellation timing, callback allocation and every failure path need separate focused coverage.',
                           'Independent mathematical/control agreement is limited to this explicit finite fixture set.']}
    write_json(args.output,result)
    print(json.dumps({'passed':result['passed'],'checks':len(checks),'rows':len(rows),'failures':failures}))
    return 0 if result['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
