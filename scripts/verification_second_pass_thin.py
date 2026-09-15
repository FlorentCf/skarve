#!/usr/bin/env python3
"""80-digit Decimal clipping audit for one ill-conditioned thin-strip fixture."""
import argparse
from decimal import Decimal, getcontext
import hashlib
import json
import math
from pathlib import Path
import sys

import numpy as np
from shapely.geometry import Polygon, box, mapping

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from benchmarks.reference import Native

getcontext().prec = 80


def clip(points, axis, bound, greater):
    if not points:
        return []
    answer = []
    a = points[-1]
    inside = lambda p: p[axis] >= bound if greater else p[axis] <= bound
    for b in points:
        if inside(a) != inside(b):
            t = (bound - a[axis]) / (b[axis] - a[axis])
            p = [a[i] + t * (b[i] - a[i]) for i in range(2)]
            p[axis] = bound
            answer.append(tuple(p))
        if inside(b):
            answer.append(b)
        a = b
    return answer


def area(points):
    if len(points) < 3:
        return Decimal(0)
    x, y = points[0]
    return abs(sum(((a[0]-x)*(b[1]-y)-(b[0]-x)*(a[1]-y) for a,b in zip(points,points[1:]+points[:1])), Decimal(0))) / 2


def run(library):
    rng = np.random.default_rng(42)
    bands = []
    for index in range(2):
        values = rng.integers(-10, 30, size=256).astype(float)
        valid = rng.uniform(size=256) > index * .1
        bands.append(dict(values=values.tolist(), valid=valid.tolist()))
    data = dict(grid=dict(width=16, height=16, transform=[0,1,0,16,0,-1], crs="LOCAL"), bands=bands)
    report = []
    with Native(library=library) as native:
        native.call(dict(op="open", id="r", raster=data))
        native.call(dict(op="prepare", source="r"))
        for epsilon in [1e-6, 1e-9, 1e-12]:
            vertices = [(1.,1.),(15.,14.),(15.,14.+epsilon),(1.,1.+epsilon)]
            polygon = Polygon(vertices)
            decimal_vertices = [tuple(Decimal.from_float(v) for v in point) for point in vertices]
            exact, geos = {}, {}
            for row in range(16):
                for col in range(16):
                    points = decimal_vertices
                    for axis,bound,greater in [(0,col,True),(0,col+1,False),(1,15-row,True),(1,16-row,False)]:
                        points = clip(points, axis, Decimal(bound), greater)
                    f = area(points)
                    if f > 0:
                        exact[row,col] = f
                    g = polygon.intersection(box(col,15-row,col+1,16-row)).area
                    if g > 0:
                        geos[row,col] = g
            request=dict(op="measure",source="r",geometry=mapping(polygon),crs="LOCAL")
            outputs={strategy:native.call(request|dict(strategy=strategy)) for strategy in ["direct","scanline"]}
            plan=native.call(dict(op="compile",source="r",id="p",geometry=mapping(polygon),crs="LOCAL",strategy="direct",debug_cells=True))
            direct_cells={(c["row"],c["col"]):c["fraction"] for c in plan["cells"]}
            native.call(dict(op="drop_plan",plan="p"))
            statistics=[]
            for index, band in enumerate(bands):
                included={k:f for k,f in exact.items() if band["valid"][k[0]*16+k[1]]}
                total=sum((Decimal.from_float(band["values"][r*16+c])*f for (r,c),f in included.items()),Decimal(0))
                support=sum(included.values(),Decimal(0))
                g_support=math.fsum(f for (r,c),f in geos.items() if band["valid"][r*16+c])
                g_total=math.fsum(band["values"][r*16+c]*f for (r,c),f in geos.items() if band["valid"][r*16+c])
                statistics.append(dict(band=index,decimal_sum=str(total),decimal_coverage=str(support),decimal_mean=str(total/support),
                                       geos_mean=g_total/g_support,geos_mean_error=g_total/g_support-float(total/support),
                                       native={strategy:dict(mean=out["bands"][index]["coverage_weighted_mean"],mean_error=out["bands"][index]["coverage_weighted_mean"]-float(total/support),sum=out["bands"][index]["fractional_sum"],coverage=out["bands"][index]["covered_cell_equivalents"],intersecting=out["bands"][index]["intersecting_cell_count"]) for strategy,out in outputs.items()}))
            report.append(dict(epsilon=epsilon,vertices=vertices,decimal_vertices=[[str(x),str(y)] for x,y in decimal_vertices],decimal_intersecting=len(exact),geos_intersecting=len(geos),native_direct_intersecting=len(direct_cells),
                               missing_native_cells=[dict(row=r,col=c,decimal_fraction=str(exact[r,c]),geos_fraction=geos.get((r,c),0)) for r,c in exact.keys()-direct_cells.keys()],
                               statistics=statistics,decimal_cell_fractions=[dict(row=r,col=c,fraction=str(f),geos_fraction=geos.get((r,c),0),native_direct_fraction=direct_cells.get((r,c),0)) for (r,c),f in exact.items()]))
    return dict(decimal_precision=80,input_interpretation="Exact binary64 submitted world coordinates via Decimal.from_float; no epsilon snapping",library_sha256=hashlib.file_digest(Path(library).open("rb"),"sha256").hexdigest(),cases=report)


if __name__ == "__main__":
    p=argparse.ArgumentParser()
    p.add_argument("--library",required=True)
    p.add_argument("--output",required=True)
    a=p.parse_args()
    result=run(a.library)
    Path(a.output).write_text(json.dumps(result,indent=2)+"\n")
    print(json.dumps([{k:v for k,v in case.items() if k not in ["decimal_cell_fractions","decimal_vertices"]} for case in result["cases"]],indent=2))
