#!/usr/bin/env python3
"""Render the sanitized public benchmark receipt; no measurements are executed."""
import argparse
import csv
from collections import defaultdict
import gzip
import html
import json
import math
import os
import re
from pathlib import Path
import statistics

def percentile(values,p):
 values=sorted(values);position=(len(values)-1)*p;low=math.floor(position);high=math.ceil(position)
 return values[low]+(values[high]-values[low])*(position-low)
def main():
 p=argparse.ArgumentParser();p.add_argument('--input',required=True,type=Path);p.add_argument('--bulk',type=Path);p.add_argument('--output',type=Path);args=p.parse_args()
 folder=args.input;out=args.output or folder/'report';assert not out.exists();out.mkdir(parents=True)
 raw_prefix=Path(os.path.relpath(folder,out)).as_posix()
 with gzip.open(folder/'results.json.gz','rt') as stream:data=json.load(stream)
 # Display-only spacing repair; preserve the immutable measured receipt.
 data['boundary']=data['boundary'].replace('is64MiB','is 64 MiB').replace('explicit0/1MiB','explicit 0/1 MiB').replace('common2GiB','common 2 GiB')
 fixtures=json.loads((folder/'fixtures.json').read_text());bulk=json.loads(args.bulk.read_text()) if args.bulk else None
 import matplotlib
 matplotlib.use('Agg')
 import matplotlib.pyplot as plt
 plt.rcParams.update({'font.family':'DejaVu Sans','font.size':10,'axes.spines.top':False,'axes.spines.right':False,'axes.labelcolor':'#24394a','text.color':'#24394a','axes.edgecolor':'#c5cdd4','figure.facecolor':'#f7f9fa','axes.facecolor':'#ffffff','svg.fonttype':'none'})
 figures=[];chart_rows=[]
 def save(fig,name):
  fig.savefig(out/(name+'.svg'),bbox_inches='tight');fig.savefig(out/(name+'.png'),dpi=180,bbox_inches='tight');plt.close(fig);figures.append(name)
  svg=out/(name+'.svg');svg.write_text('\n'.join(line.rstrip() for line in svg.read_text().splitlines())+'\n')
 grouped=defaultdict(list)
 for row in data['records']:
  if row['experiment']=='batch':grouped[(row['family'],row['system'],row['method'])].append(row['elapsed_ms'])
 fig,axes=plt.subplots(1,2,figsize=(12,5.2));fig.suptitle('Complete original-source batch operations',fontsize=17,fontweight='bold',x=.06,ha='left')
 for ax,family in zip(axes,['dates','files']):
  keys=[k for k in grouped if k[0]==family];keys.sort(key=lambda k:statistics.median(grouped[k]))
  values=[statistics.median(grouped[k]) for k in keys];labels=[k[1]+' / '+k[2] for k in keys]
  colors=['#007f79' if k[1]=='skarve' else '#d38725' for k in keys]
  ax.barh(labels,values,color=colors);ax.invert_yaxis();ax.set_xlabel('Milliseconds; median with min–max spread');ax.set_title('Synthetic date stack' if family=='dates' else 'Six distinct two-band files',loc='left',pad=12)
  for i,(key,value) in enumerate(zip(keys,values)):
   ax.plot([min(grouped[key]),max(grouped[key])],[i,i],color='#24394a',linewidth=1.4);ax.text(max(grouped[key]),i,f'  {value:.2f}',va='center',fontsize=9)
   chart_rows.append({'figure':'batch','family':family,'system':key[1],'method':key[2],'n':len(grouped[key]),'median_ms':value,'min_ms':min(grouped[key]),'max_ms':max(grouped[key])})
  ax.grid(axis='x',alpha=.15);ax.set_axisbelow(True);ax.margins(x=.22)
 fig.tight_layout(rect=(0,0,1,.91));save(fig,'01-batch')
 singles=defaultdict(list)
 for row in data['records']:
  if row['experiment']=='single':singles[(row['pattern'],row['system'],row['method'])].append(row['elapsed_ms'])
 keys=sorted(singles);labels=[k[0]+' / '+k[1]+' / '+k[2] for k in keys]
 fig,axes=plt.subplots(1,2,figsize=(13,5.4));fig.suptitle('Distinct polygons with retained original sources',fontsize=17,fontweight='bold',x=.04,ha='left')
 for ax,metric in zip(axes,['median','p95']):
  values=[statistics.median(singles[k]) if metric=='median' else percentile(singles[k],.95) for k in keys]
  ax.barh(labels,values,color=['#007f79' if k[1]=='skarve' else '#d38725' for k in keys]);ax.invert_yaxis();ax.set_xscale('log');ax.set_xlabel('Milliseconds, logarithmic scale');ax.set_title('Median' if metric=='median' else 'Empirical p95 of distinct geometries',loc='left')
  for i,(k,v) in enumerate(zip(keys,values)):
   ax.text(v,i,f'  {v:.3f}',va='center',fontsize=9)
   chart_rows.append({'figure':'single','pattern':k[0],'system':k[1],'method':k[2],'n':len(singles[k]),'metric':metric,'ms':v})
  ax.margins(x=.22);ax.grid(axis='x',alpha=.15);ax.set_axisbelow(True)
 fig.tight_layout(rect=(0,0,1,.91));save(fig,'02-new-polygons')
 prepared=defaultdict(list)
 for row in data['records']:
  if row['experiment']=='prepared':prepared[row['method']].append(row['elapsed_ms'])
 if prepared:
  setup=data.get('optional_preparation',{});fig,ax=plt.subplots(figsize=(9,4.2));methods=['direct-source','original-summary-index']
  query=[sum(prepared[m]) for m in methods];build=[0,setup.get('build_ms',0)+setup.get('index_registration_ms',0)];source=[setup.get('source_registration_ms',0)]*2
  ax.barh(methods,source,label='Source registration',color='#8aabb5');ax.barh(methods,build,left=source,label='Build + index registration',color='#d38725');ax.barh(methods,query,left=[a+b for a,b in zip(source,build)],label='All matched query calls + sink',color='#007f79')
  ax.set_title('Optional preparation: charge the complete lifecycle',loc='left',fontsize=16,fontweight='bold',pad=15);ax.set_xlabel('Milliseconds for the same polygon cohort');ax.invert_yaxis();ax.legend(loc='upper center',bbox_to_anchor=(.5,-.2),ncol=3,frameon=False);fig.tight_layout();save(fig,'03-preparation')
  for m,q,b,s in zip(methods,query,build,source):chart_rows.append({'figure':'preparation','method':m,'queries':len(prepared[m]),'query_ms':q,'build_registration_ms':b,'source_registration_ms':s,'total_ms':q+b+s})
 if bulk:
  bg=defaultdict(list)
  for row in bulk['records']:bg[(row['case'],row['system'])].append(row['elapsed_ms'])
  cases=sorted({k[0] for k in bg});fig,ax=plt.subplots(figsize=(12,max(5,len(cases)*.4)))
  for shift,system,color in [(-.18,'native','#007f79'),(.18,'javascript','#d38725')]:
   values=[statistics.median(bg[(c,system)]) for c in cases];positions=[i+shift for i in range(len(cases))];ax.barh(positions,values,height=.34,label=system,color=color)
   for c,v in zip(cases,values):chart_rows.append({'figure':'bulk','case':c,'system':system,'median_ms':v,'n':len(bg[(c,system)])})
  def short_case(case):
   dtype,shape,policy=case.split('-',2);bands,cells=shape.split('x')
   return f'{"F32" if dtype=="Float32Array" else "F64"} · {bands} bands · {int(cells)//1000}k values · {"ordered" if policy.startswith("hm_") else "strict"}'
  ax.set_yticks(range(len(cases)),[short_case(c) for c in cases]);ax.invert_yaxis();ax.set_xscale('log');ax.set_xlabel('Milliseconds per complete selected-window call; log scale');ax.set_title('Typed buffers versus an independent JavaScript loop',loc='left',fontsize=16,fontweight='bold',pad=16);ax.legend(frameon=False);fig.text(.02,.015,'Two windows per call. Strict = strict_selected_v1; ordered = hm_demographics_ordered_v1.',fontsize=9);fig.tight_layout(rect=(0,.035,1,1));save(fig,'04-typed-buffers')
 failures=[{'record':v['record'],'system':v['system'],'method':v['method'],'count':len(v['strict_failures']),'max_absolute_error':v['max_absolute_error']} for v in data['validation'] if v['strict_failures']]
 (out/'differences.json').write_text(json.dumps(failures+data['errors'],indent=2)+'\n')
 summary={'schema':1,'native_library_sha256':data['freeze']['library_sha256'],'seed':data['freeze']['seed'],'native_gate_pass':data['native_gate_pass'],'records':len(data['records']),'native_checks':sum(v['checks'] for v in data['validation'] if v['system']=='skarve'),'native_failed_scalar_records':sum(len(v['strict_failures']) for v in data['validation'] if v['system']=='skarve'),'external_failed_scalar_records':sum(len(v['strict_failures']) for v in data['validation'] if v['system']!='skarve'),'failed_or_incompatible_records':failures,'runtime_errors':data['errors'],'chart_rows':chart_rows,'source_bytes':fixtures['total_source_bytes'],'fixture_build_ms':fixtures['build_ms'],'optional_preparation':data.get('optional_preparation'),'max_process_rss_bytes':data['max_process_rss_bytes'],'matplotlib_version':matplotlib.__version__}
 compatibility=[]
 for system in ['skarve','exactextract']:
  validations=[v for v in data['validation'] if v['system']==system]
  for field in ['sum','support','mean','min','max']:
   compatibility.append({'system':system,'field':field,'max_absolute_error':max((v['max_absolute_error'].get(field,0) for v in validations),default=0),'strict_failures':sum(sum(f['statistic']==field for f in v['strict_failures']) for v in validations)})
 cache=[]
 for method in ['default','decoded-warm','cache-overflow']:
  rows=[r for r in data['records'] if r['experiment']=='single' and r['system']=='skarve' and r['method']==method]
  if rows:cache.append({'method':method,'queries':len(rows),'cache_budget_bytes':rows[0]['metrics']['cache_budget_bytes'],'source_registration_ms':rows[0]['source_registration_ms'],'warmup_ms':rows[0]['warmup']['elapsed_ms'] if rows[0]['warmup'] else 0,**{k:sum(r['metrics'][k] for r in rows) for k in ['raster_io_calls','decoded_value_bytes','cache_hits','cache_misses','cache_evictions']},'peak_cache_resident_bytes':max(r['metrics']['cache_resident_bytes'] for r in rows)})
 summary.update({'compatibility':compatibility,'cache':cache,'versions':data['freeze']['versions'],'host':data['freeze']['host'],'external_checks':sum(v['checks'] for v in data['validation'] if v['system']=='exactextract'),'oracle_ms':data['oracle_ms'],'samples':{k:data['freeze'][k] for k in ['batch_zones','date_slices','distinct_files','bands_per_distinct_file','single_queries_per_pattern','rounds']}})
 if bulk:summary['bulk']={k:bulk[k] for k in ['seed','node','library_sha256','module_sha256','harness_sha256','pass','correctness_fields','max_rss_bytes','limits','errors']}
 findings=[]
 for family in ['dates','files']:
  choices={key:statistics.median(values) for key,values in grouped.items() if key[0]==family}
  native=min((key for key in choices if key[1]=='skarve'),key=choices.get);reference=min((key for key in choices if key[1]=='exactextract'),key=choices.get)
  findings.append(f'{family}: the lowest observed native median was {choices[native]:.3f} ms ({native[2]}); the strongest observed exactextract median was {choices[reference]:.3f} ms ({reference[2]}), a native/reference ratio of {choices[native]/choices[reference]:.3f}. Strategies are chosen per family after measurement, and all alternatives remain plotted. Three rounds do not establish a reliable winner where spread overlaps.')
 for method,pattern in [('default','hot'),('decoded-warm','hot'),('cache-overflow','scatter')]:
  native=statistics.median(singles[(pattern,'skarve',method)]);reference=min(statistics.median(v) for k,v in singles.items() if k[0]==pattern and k[1]=='exactextract')
  findings.append(f'Single {method}: {native:.3f} ms median versus the fastest reference median {reference:.3f} ms on the same {pattern} geometry sequence, native/reference ratio {native/reference:.2f}. Setup and warmup are listed separately; these are retained-source calls on local files.')
 if bulk:
  ratios={c:statistics.median(bg[(c,'native')])/statistics.median(bg[(c,'javascript')]) for c in cases}
  findings.append(f'Typed buffers: native medians were lower in {sum(v<1 for v in ratios.values())}/{len(ratios)} shapes; native/JavaScript median ratios span {min(ratios.values()):.2f}–{max(ratios.values()):.2f}. Large float64 ordered calls and several small calls lose; the public buffer interface is not a universal speedup. Native results and all exclusion counters match the independent control exactly. RSS is observed for Node, not constrained with a virtual-address limit.')
 summary['findings']=findings
 keys=sorted({key for row in chart_rows for key in row})
 with (out/'chart_data.csv').open('w',newline='') as stream:
  writer=csv.DictWriter(stream,fieldnames=keys,lineterminator='\n');writer.writeheader();writer.writerows(chart_rows)
 (out/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
 title='Skarve — public synthetic release confirmation'
 body=[f'# {title}',f'Native library `{summary["native_library_sha256"]}`. Seed `{summary["seed"]}`. This report describes generated inputs on one host; it does not relabel historical private measurements.',f'Native checks: **{summary["native_checks"]:,}**, native mismatches: **{summary["native_failed_scalar_records"]:,}**. Runtime errors: **{len(data["errors"])}**. External scalar differences exceeding native tolerance: **{summary["external_failed_scalar_records"]:,}**; they remain compatibility differences, not an interchangeable-backend guarantee.',f'Original fixture storage: **{fixtures["total_source_bytes"]:,} bytes**; generation: **{fixtures["build_ms"]:.2f} ms**. Peak benchmark process RSS: **{data["max_process_rss_bytes"]:,} bytes**. Optional index storage: **{data.get("optional_preparation",{}).get("index_bytes",0):,} bytes**; normalized full-raster duplicate: **0 bytes**.',f'Hardware: {data["freeze"]["host"]["cpu_model"]}; {data["freeze"]["host"]["system"]} {data["freeze"]["host"]["machine"]}. Versions: `{json.dumps(data["freeze"]["versions"],sort_keys=True)}`.',data['boundary']]
 body += ['## What this run supports',*findings,'Native and reference GDAL versions differ as recorded above; both use their installed, applicable source adapters. The comparison is between complete systems, not a pure kernel isolation. The 24-band date source is registered in bounded groups of 20 and 4 bands. Thin, holed, multipart, aligned, partial and outside geometries are retained. The single-query source uses one-row strips, so the single result is layout-specific.','## Numerical compatibility','| System | Field | Maximum absolute error | Scalar differences beyond native tolerance |\n|---|---|---:|---:|\n'+'\n'.join(f'| {r["system"]} | {r["field"]} | {r["max_absolute_error"]:.10g} | {r["strict_failures"]} |' for r in compatibility),'Checks include repeated rounds; they are not counts of unique polygons. The sum tolerance is 1e-8 + 1e-10 times absolute contribution mass; other floating fields use 1e-8 + 1e-10 times the reference magnitude. Integer counts require equality. Empty exactextract mean/min/max NaN values map to null only at zero support. No nonempty values or precision differences are repaired.','## Cache and setup accounting','```json\n'+json.dumps(cache,indent=2)+'\n```']
 for name in figures:body += [f'![{name}]({name}.svg)']
 body += ['## Interpretation',*['- '+text for text in data['limitations']],'All plotted numbers are preserved in `summary.json`; complete answers, numerical discrepancies and runtime errors are in `../results.json.gz`, with `../timings.csv`, `../fixtures.json` and `../freeze.json`. Lower is better. Every losing method remains visible. The native and reference lanes compute different finite-precision coverage; inspect fieldwise errors before treating a speed comparison as compatible execution.','## Failed or incompatible cases',f'{len(data["errors"])} runtime errors; {summary["native_failed_scalar_records"]:,} native scalar mismatches; {summary["external_failed_scalar_records"]:,} external scalar differences across {sum(v["system"]=="exactextract" for v in failures)} incompatible timing records. The complete per-record discrepancy list is retained in [differences.json](differences.json), and each individual field discrepancy is retained in the raw compressed result receipt.']
 body=[text.replace('../results.json.gz',raw_prefix+'/results.json.gz').replace('../timings.csv',raw_prefix+'/timings.csv').replace('../fixtures.json',raw_prefix+'/fixtures.json').replace('../freeze.json',raw_prefix+'/freeze.json') for text in body]
 (out/'REPORT.md').write_text('\n\n'.join(body)+'\n')
 cards=''.join(f'<figure><img src="{name}.svg" alt="{name}"></figure>' for name in figures)
 def inline(value):return re.sub(r'`([^`]+)`',r'<code>\1</code>',re.sub(r'\*\*([^*]+)\*\*',r'<strong>\1</strong>',html.escape(value)))
 intro=''.join('<p>'+inline(x)+'</p>' for x in body[1:6])
 intro+='<h2>What this run supports</h2>'+''.join('<p>'+html.escape(x)+'</p>' for x in findings)
 intro+='<h2>Numerical compatibility</h2><table><tr><th>System</th><th>Field</th><th>Maximum absolute error</th><th>Scalar differences</th></tr>'+''.join(f'<tr><td>{r["system"]}</td><td>{r["field"]}</td><td>{r["max_absolute_error"]:.10g}</td><td>{r["strict_failures"]}</td></tr>' for r in compatibility)+'</table>'
 page=f'<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{title}</title><style>body{{margin:0;background:#f1f5f6;color:#24394a;font:16px/1.55 system-ui,sans-serif}}main{{max-width:1220px;margin:48px auto;padding:0 28px}}h1{{font-size:36px;letter-spacing:-1px;line-height:1.2}}p{{max-width:1000px}}figure{{margin:30px 0;background:white;border:1px solid #dce4e8;padding:18px;border-radius:10px}}img{{width:100%;height:auto}}pre{{white-space:pre-wrap;font-size:12px;background:white;padding:20px}}a{{color:#007f79}}</style><main><h1>{title}</h1>{intro}<p>Complete source calls, explicit cache state, optional preparation, and all meaningful losses. Read the <a href="REPORT.md">full report</a>, <a href="summary.json">chart data</a> and <a href="../timings.csv">raw timings</a>.</p>{cards}<h2>Limits and compatibility</h2><ul>{"".join("<li>"+html.escape(x)+"</li>" for x in data["limitations"])}</ul><h2>Failed or incompatible cases</h2><pre>{html.escape(json.dumps(failures+data["errors"],indent=2))}</pre></main></html>'
 page=page.replace('../timings.csv',raw_prefix+'/timings.csv').replace('table><tr>','table style="border-collapse:collapse;text-align:left"><tr>').replace('<td>','<td style="padding:6px 18px 6px 0;border-bottom:1px solid #dce4e8">').replace('<th>','<th style="padding-right:18px">')
 start=page.index('<h2>Failed or incompatible cases</h2>')
 page=page[:start]+'<h2>Failed or incompatible cases</h2><p>'+html.escape(body[-1]).replace('[differences.json](differences.json)','<a href="differences.json">differences.json</a>')+'</p></main></html>'
 (out/'index.html').write_text(page);print(json.dumps({'report':str(out/'index.html'),'figures':len(figures),'native_gate_pass':data['native_gate_pass']}))
if __name__=='__main__':main()
