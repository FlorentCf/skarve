"""Fresh pixel-coordinate workloads, transformed once to the native source grid."""
from __future__ import annotations
import random

def rectangle(x0,y0,x1,y1):return [[x0,y0],[x1,y0],[x1,y1],[x0,y1],[x0,y0]]

def zones(grid,seed=319517):
    randomizer=random.Random(seed);w,h=grid['width'],grid['height'];tx,dx,_,ty,_,dy=grid['transform']
    cx=w*.48+randomizer.random()*3;cy=h*.49+randomizer.random()*3
    unit=min(w,h);margin=min(15.125+randomizer.random()*4,unit*.06)
    jitter=randomizer.random()*.8;lo=.33+randomizer.random()*.03;hi=.64+randomizer.random()*.03
    polygons=[('tiny','Polygon',[rectangle(cx+.125,cy+.125,cx+.375,cy+.375)]),
        ('compact','Polygon',[rectangle(cx-11.375,cy-9.125,cx+13.375,cy+10.625)]),
        ('interior','Polygon',[rectangle(margin,margin,w-margin-.375,h-margin-.625)]),
        ('boundary','Polygon',[[[margin,margin],[w-margin,h-margin-2.125],[w-margin,h-margin],[margin,margin+2.125],[margin,margin]]]),
        ('hole','Polygon',[rectangle(margin,margin,w-margin,h-margin),rectangle(w*lo,h*lo,w*hi,h*hi)[::-1]]),
        ('multipart','MultiPolygon',[[rectangle(3.125+jitter,4.375+jitter,w*(.22+jitter*.02),h*.27)],[rectangle(w*.71,h*(.72+jitter*.02),w-4.625-jitter,h-3.375-jitter)]]),
        ('edge','Polygon',[rectangle(-.375-jitter,-.625-jitter,23.375+jitter,27.125+jitter)]),
        ('outside','Polygon',[rectangle(w+1.125+jitter,h+2.375+jitter,w+4.875+jitter,h+6.625+jitter)])]
    def coordinates(value):
        if len(value)==2 and isinstance(value[0],(int,float)):return [tx+value[0]*dx,ty+value[1]*dy]
        return [coordinates(v) for v in value]
    return [{'id':name,'version':str(seed),'geometry':{'type':kind,'coordinates':coordinates(points)}} for name,kind,points in polygons]

def workload(grid,case,seed=319517,single_family='interior',many=8):
    all_zones=zones(grid,seed)
    if case in ('A','C'):return [next(z for z in all_zones if z['id']==single_family)]
    if many<=8:return all_zones[:many]
    result=[]
    for offset in range((many+7)//8):
        for z in zones(grid,seed+offset*991):
            z['id']+=f'-{offset:02d}';result.append(z)
    return result[:many]
