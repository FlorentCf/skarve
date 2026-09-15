#!/usr/bin/env python3
"""Independent exact round-trip of raw SKV with globally ineligible summaries."""
import argparse,json,os,resource,sys
from pathlib import Path
for name in ('OMP_NUM_THREADS','OPENBLAS_NUM_THREADS','MKL_NUM_THREADS'):os.environ[name]='1'
from skv_format_oracle import Oracle

def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--library',type=Path,required=True);p.add_argument('--output',type=Path,required=True);p.add_argument('--checkout-bindings',action='store_true');args=p.parse_args()
    resource.setrlimit(resource.RLIMIT_AS,(2<<30,2<<30))
    import numpy as np
    import rasterio
    if args.checkout_bindings:sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'bindings/python'))
    from skarve import Skarve
    assert not args.output.exists();args.output.mkdir(parents=True)
    source=args.output/'overflow.tif';target=args.output/'overflow.skv'
    with rasterio.open(source,'w',driver='GTiff',width=128,height=2,count=2,dtype='float64',crs='EPSG:3857',transform=rasterio.transform.from_origin(0,2,1,1)) as output:
        output.write(np.full((2,128),1e308,dtype='float64'),1)
        output.write(np.arange(256,dtype='float64').reshape(2,128)/16,2)
    with Skarve(args.library) as engine:
        with engine.infuse(str(source)) as raster:conversion=raster.compile(str(target),chunk_edge=64)
    assert conversion['summaries'] is False and conversion['summary_disabled_reason']
    with Oracle(target) as oracle:
        structure=oracle.verify_payloads();comparison=oracle.compare_source(source)
        states=sum(bool(record.flags&2) for record in oracle.records)
        assert states>0,'Fixture must retain some representable states while disabling summary serving globally'
    receipt={'schema':'skarve_skv_overflow_oracle_v1','passed':True,'global_summaries':False,'representable_states_retained':states,'structure':structure,'source_comparison':comparison,'reason':conversion['summary_disabled_reason'],'scope':'Lossless raw values remain servable when finite samples have an unrepresentable aggregate; partial stored summaries do not imply global summary eligibility.'}
    (args.output/'receipt.json').write_text(json.dumps(receipt,indent=2)+'\n');print(json.dumps({'passed':True,'representable_states_retained':states,'output':str(args.output)}))

if __name__=='__main__':main()
