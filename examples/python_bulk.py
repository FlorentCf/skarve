#!/usr/bin/env python3
"""Typed buffers are a separate supplied-selection contract; no raster is needed."""
import json
import numpy as np
from skarve import Engine


def run():
    answers=[]
    with Engine() as engine:
        for count in (36,40):
            values=np.array([1.25,-2.,0.,3.75],dtype=np.float32)
            windows=[{'bands':[{'id':100+band,'values':values} for band in range(count)],
                      'selection':np.array([0,1,2,3],dtype=np.uint32)}]
            strict=engine.bulk_reduce(windows)
            ordered=engine.bulk_reduce(windows,policy='hm_demographics_ordered_v1')
            assert all(b['sum']==3 and b['valid_count']==4 for b in strict['bands'])
            assert all(b['sum']==5 and b['valid_count']==3 for b in ordered['bands'])
            assert [b['id'] for b in strict['bands']]==list(range(100,100+count))
            answers.append({'bands':count,'strict_sum':3,'ordered_sum':5,
                            'copied_bytes':strict['metadata']['owned_copy_bytes']})
    return {'interface':'Python','cases':answers,'source_identity_claimed':False}


if __name__=='__main__':print(json.dumps(run()))
