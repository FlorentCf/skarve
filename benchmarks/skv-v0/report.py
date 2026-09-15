#!/usr/bin/env python3
"""Render source-backed, standalone SKV research figures and exact CSV tables.

No benchmark is executed here. All failures and numerical incompatibilities
remain visible. An incomplete group is never presented as a successful median.
"""
from __future__ import annotations
import argparse,csv,hashlib,json,statistics,textwrap
from collections import defaultdict
from pathlib import Path

LABELS={
 'native-ordinary':'TIFF stripped / BAND','native-cog128':'COG128 / PIXEL','native-cog256':'COG256 / PIXEL',
 'native-band256':'TIFF tiled256 / BAND','native-cog-index':'COG256 + RSI256','native-cog-index64':'COG256 + RSI64',
 'native-cogband128':'COG128 / BAND','native-cogtile128':'COG128 / TILE',
 'native-cogband-index':'COG BAND + RSI256','native-cogband-index64':'COG BAND + RSI64',
 'native-cogtile-index':'COG TILE + RSI256','native-cogtile-index64':'COG TILE + RSI64',
 'native-skv-raw':'SKV / summaries disabled','native-skv-summary':'SKV / summaries enabled'}
CASES={'A':'A · one polygon / one band','B':'B · many polygons / one band','C':'C · one polygon / all bands','D':'D · many polygons / all bands'}

def sha(path):
 with Path(path).open('rb') as stream:return hashlib.file_digest(stream,'sha256').hexdigest()

def label(lane):
 if lane in LABELS:return LABELS[lane]
 return lane.replace('upstream-','Upstream EE · ').replace('ee-','Skarve EE · ').replace('-',' ')

def color(lane):
 if lane=='native-skv-summary':return '#235d86'
 if lane=='native-skv-raw':return '#9cb8cb'
 if lane.startswith('upstream'):return '#bc8c37'
 if lane.startswith('ee-'):return '#667c4a'
 return '#76868f'

def write_csv(path,rows):
 if not rows:return
 keys=list(dict.fromkeys(k for row in rows for k in row))
 with path.open('w',newline='') as stream:
  writer=csv.DictWriter(stream,keys);writer.writeheader();writer.writerows(rows)

def main():
 p=argparse.ArgumentParser(description=__doc__);p.add_argument('--programmes',type=Path,nargs='+',required=True);p.add_argument('--prepared',type=Path,nargs='*',default=[]);p.add_argument('--output',type=Path,required=True);p.add_argument('--stage',choices=['development','final'],required=True);args=p.parse_args()
 assert not args.output.exists(),'Fresh report destination required';args.output.mkdir(parents=True)
 receipts=[];records=[];failures=[];groups=defaultdict(list);points=[];setup=[];cohort_owners={};numerical=[];cross_policy=[]
 for path in args.programmes:
  programme_sha=sha(path);data=json.loads(path.read_text());freeze_path=path.with_name('freeze.json');freeze=json.loads(freeze_path.read_text());assert sha(freeze_path)==data['freeze_sha256']
  validation={row['task']:row for row in data['format_validation']}
  lookup={(r['task']['dataset'],r['task']['case'],r['task']['round'],r['task']['regime'],r['task']['lane']):r for r in data['records']}
  receipts.append({'programme_sha256':programme_sha,'freeze_sha256':sha(freeze_path),'library_sha256':freeze['library_sha256'],'bindings_mode':freeze['bindings_mode'],'tasks':len(data['records']),'passed':data['passed']})
  setup.append(data.get('fixture_integrity_setup',{}))
  for record in data['records']:
   task=record['task'];check=validation.get(task['id'],{});valid=record['passed'] and check.get('passed',False)
   cohort=(task['dataset'],task['regime'],task['case'],task['lane'])
   assert cohort not in cohort_owners or cohort_owners[cohort]==programme_sha,'Overlapping cohorts from different programmes: render configurations separately instead of averaging across corrections'
   cohort_owners[cohort]=programme_sha
   row={'programme_sha256':programme_sha,'task':task['id'],'dataset':task['dataset'],'case':task['case'],'regime':task['regime'],'lane':task['lane'],'round':task['round'],'zones':len(task['zones']),'bands':len(task['bands']),'execution_passed':record['passed'],'numerical_passed':check.get('passed',False),'source_to_consumed_ms':record.get('source_to_consumed_ms'),'process_wall_ms':record.get('process_wall_ms'),'source_open_ms':record.get('source_open_ms'),'index_open_ms':record.get('index_open_ms'),'query_ms':record.get('query_ms'),'sink_ms':record.get('sink',{}).get('ms'),'peak_rss_bytes':record.get('peak_rss_bytes'),'requests':record.get('http',{}).get('requests'),'body_bytes':record.get('http',{}).get('body_bytes'),'primary_requests':record.get('http',{}).get('primary_requests'),'primary_body_bytes':record.get('http',{}).get('primary_body_bytes'),'error':json.dumps(record.get('error') or check.get('differences') or check.get('reason') or None,sort_keys=True)}
   records.append(row);groups[(task['dataset'],task['regime'],task['case'],task['lane'])].append((valid,row))
   policy=(record.get('provenance') or {}).get('numerical_policy',record.get('policy','native_grid_planar_fractional' if task['backend']=='native' else 'exactextract_fractional_v030'))
   numerical.append({'task':task['id'],'dataset':task['dataset'],'case':task['case'],'lane':task['lane'],'policy':policy,'execution_passed':record['passed'],'numerical_passed':check.get('passed',False),'control':check.get('control'),'checks':check.get('checks'),'tolerance':'absolute 1e-8 + 1e-10 * abs(reference), as frozen common.py; matching null/finite shape' if task['backend']=='native' else 'zero; matching null/finite shape','cross_policy_equivalence_claim':False})
   if not valid:failures.append(row)
  for record in data['records']:
   task=record['task']
   if task['backend']=='native' or not record['passed']:continue
   control=lookup.get((task['dataset'],task['case'],task['round'],task['regime'],freeze.get('native_reference','native-ordinary')))
   if control is None or not control['passed']:continue
   expected={row['zone']:row['bands'] for row in control['answers']}
   for field in ('sum','support','mean','min','max'):
    errors=[];relative=[];null_mismatches=0
    for answer in record['answers']:
     for actual,native in zip(answer['bands'],expected[answer['zone']]):
      a,b=actual[field],native[field]
      if a is None or b is None:null_mismatches+=a!=b;continue
      errors.append(abs(a-b))
      if b!=0:relative.append(abs(a-b)/abs(b))
    cross_policy.append({'task':task['id'],'dataset':task['dataset'],'case':task['case'],'lane':task['lane'],'regime':task['regime'],'round':task['round'],'native_control':control['task']['id'],'field':field,'finite_pairs':len(errors),'null_mismatches':null_mismatches,'max_absolute_difference':max(errors,default=0),'max_relative_difference_nonzero_native':max(relative,default=0),'scope':'Descriptive cross-policy difference, never an eligibility or same-policy correctness gate'})
 import matplotlib
 matplotlib.use('Agg')
 import matplotlib.pyplot as plt
 plt.rcParams.update({'font.family':'DejaVu Sans','font.size':10,'text.color':'#24394a','axes.labelcolor':'#24394a','axes.edgecolor':'#bdc7cd','axes.spines.top':False,'axes.spines.right':False,'figure.facecolor':'white','axes.facecolor':'white','svg.fonttype':'none','axes.titleweight':'bold'})
 figures=[]
 def save(fig,name,note):
  fig.text(.02,.012,'\n'.join(textwrap.wrap(note,170)),ha='left',va='bottom',fontsize=9,color='#425868')
  fig.savefig(args.output/(name+'.svg'),bbox_inches='tight');fig.savefig(args.output/(name+'.png'),dpi=170,bbox_inches='tight');plt.close(fig)
  figures.append({'id':name,'png_sha256':sha(args.output/(name+'.png')),'svg_sha256':sha(args.output/(name+'.svg')),'note':note})
 def panel(ax,dataset,regime,case,lanes,metric,multiplier=1):
  maximum=0;labels=[]
  for y,lane in enumerate(lanes):
   group=groups.get((dataset,regime,case,lane),[]);labels.append(label(lane))
   valid=bool(group) and all(ok and row.get(metric) is not None for ok,row in group)
   if not valid:
    status='FAILED / incompatible' if group else 'Not measured';ax.annotate(status,(0,y),xytext=(4,0),textcoords='offset points',color='#7e4a32',fontsize=8,va='center');continue
   values=[row[metric]*multiplier for _,row in group];median=statistics.median(values);lo=min(values);hi=max(values);maximum=max(maximum,hi)
   ax.barh(y,median,height=.57,color=color(lane));ax.plot([lo,hi],[y,y],color='#233a4b',lw=1.15)
   ax.annotate(f'{median:,.2f}',(hi,y),xytext=(4,0),textcoords='offset points',va='center',fontsize=8)
   points.append({'dataset':dataset,'regime':regime,'case':case,'lane':lane,'metric':metric,'multiplier':multiplier,'median':median,'min':lo,'max':hi,'n':len(values)})
  ax.set_yticks(range(len(lanes)),labels);ax.set_ylim(len(lanes)-.5,-.5);ax.set_xlim(0,max(maximum*1.31,1));ax.grid(axis='x',alpha=.18);ax.set_axisbelow(True);ax.set_title(CASES[case],loc='left',fontsize=11)
  return ax
 for dataset,regime in sorted({(key[0],key[1]) for key in groups}):
  all_lanes=list(dict.fromkeys(row['lane'] for row in records if row['dataset']==dataset and row['regime']==regime))
  for family in ('native','external'):
   lanes=sorted([lane for lane in all_lanes if lane.startswith('native')==(family=='native')],key=lambda lane:(lane.startswith('native-skv'),lane))
   if not lanes:continue
   fig,axes=plt.subplots(1,4,figsize=(23,max(5.2,len(lanes)*.39+2.5)));fig.suptitle(f'{dataset} · {regime} · complete cold-source operation',x=.02,ha='left',fontsize=20,fontweight='bold')
   for ax,case in zip(axes,'ABCD'):panel(ax,dataset,regime,case,lanes,'source_to_consumed_ms');ax.set_xlabel('Milliseconds · lower is faster')
   fig.tight_layout(rect=(0,.095,1,.91));save(fig,f'{dataset}-{regime}-{family}-latency',f'{args.stage.upper()} · median with observed min–max, not a confidence interval; n is retained per row in chart-points.csv. Independent panel scales. Native strict and exactextract fractional policies are distinct. Fresh process and reader caches; OS cache uncontrolled. No WAN or cold-disk claim. Complete result includes all five metrics.')
  if regime!='local':
   lanes=sorted([lane for lane in all_lanes if lane.startswith('native')],key=lambda lane:(lane.startswith('native-skv'),lane))
   if lanes:
    fig,axes=plt.subplots(2,4,figsize=(23,max(8,len(lanes)*.70+3)));fig.suptitle(f'{dataset} · {regime} · observed primary query traffic',x=.02,ha='left',fontsize=20,fontweight='bold')
    for col,case in enumerate('ABCD'):
     panel(axes[0,col],dataset,regime,case,lanes,'primary_requests');axes[0,col].set_xlabel('Requests · HEAD + GET')
     panel(axes[1,col],dataset,regime,case,lanes,'primary_body_bytes',1/(1<<20));axes[1,col].set_xlabel('Transferred body · MiB')
    fig.tight_layout(rect=(0,.065,1,.94));save(fig,f'{dataset}-{regime}-traffic',f'{args.stage.upper()} · actual loopback requests and delivered bytes, source opening through consumed answer. Index HTTP traffic included. Same SKV bytes with summaries disabled/enabled. Independent scales, medians with observed ranges. Full lifecycle traffic and errors retained in measurements.csv; no R2 confirmation is implied.')
 storage=[]
 for path in args.prepared:
  for source in json.loads(path.read_text())['sources']:
   for name,variant in source['variants'].items():
    conversion=variant.get('conversion',{})
    storage.append({'dataset':source['id'],'variant':name,'bytes':variant['bytes'],'build_seconds':variant.get('conversion_wall_seconds',variant.get('elapsed_seconds')),'build_scope':'Source registration + complete conversion + close' if 'conversion_wall_seconds' in variant else ('GDAL source open + encoding + close; independent bitwise fixture check separate' if 'elapsed_seconds' in variant else 'Original fixture; encoding duration not recorded'),'metadata_bytes':conversion.get('bootstrap_bytes'),'directory_bytes':conversion.get('directory_bytes'),'encoded_payload_bytes':conversion.get('encoded_payload_bytes'),'raw_sample_bytes':conversion.get('sample_bytes'),'mask_bytes':conversion.get('mask_bytes'),'source_read_decode_ms':conversion.get('source_read_decode_ms'),'compression_ms':conversion.get('compression_ms'),'predictor_encode_ms':conversion.get('predictor_encode_ms'),'summary_ms':conversion.get('summary_ms'),'working_bound_bytes':conversion.get('working_bound_bytes'),'peak_scratch_file_bytes':conversion.get('peak_scratch_file_bytes'),'source_retained_bytes':conversion.get('source_retained_bytes'),'update_contract':'Immutable object; changes require full rebuild. No incremental update savings claimed.'})
    for key,index in variant.items():
     if key.startswith('index') and isinstance(index,dict):storage.append({'dataset':source['id'],'variant':name+' + '+key,'bytes':variant['bytes']+index['bytes'],'auxiliary_bytes':index['bytes'],'build_seconds':index['preparation_wall_seconds'],'build_scope':'Additional summary preparation; source encoding separate'})
 for dataset in sorted({row['dataset'] for row in storage}):
  selected=[row for row in storage if row['dataset']==dataset];fig,ax=plt.subplots(figsize=(11,max(4,len(selected)*.36+2)));ys=range(len(selected))
  ax.barh(ys,[row['bytes']/(1<<20) for row in selected],color=['#235d86' if row['variant'].startswith('skv') else '#76868f' for row in selected]);ax.set_yticks(ys,[row['variant'] for row in selected]);ax.invert_yaxis();ax.set_xlabel('MiB · complete serving source plus auxiliary summary objects');ax.set_xlim(left=0);ax.grid(axis='x',alpha=.18);ax.set_axisbelow(True);ax.set_title(dataset+' · complete serving storage',loc='left',fontsize=16)
  fig.tight_layout(rect=(0,.11,1,1));save(fig,dataset+'-storage','Actual byte lengths. Original source is not required by SKV serving. SKV raw/summary query ablations use the identical summary-bearing file. Build durations and their boundaries are provided in lifecycle-costs.csv; no unmeasured amortization or update-cost claim.')
 write_csv(args.output/'measurements.csv',records);write_csv(args.output/'failures.csv',failures);write_csv(args.output/'chart-points.csv',points);write_csv(args.output/'lifecycle-costs.csv',storage)
 write_csv(args.output/'numerical-compatibility.csv',numerical);write_csv(args.output/'cross-policy-differences.csv',cross_policy)
 summary={'schema':'skarve_skv_figures_v1','stage':args.stage,'receipts':receipts,'records':len(records),'failed_or_incompatible':len(failures),'figures':figures,'fixture_integrity_setup':setup,'matplotlib':matplotlib.__version__,'visual_verification':'Pending actual rendered inspection; this script does not claim visual QA.'}
 (args.output/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
 print(json.dumps({'figures':len(figures),'records':len(records),'failed_or_incompatible':len(failures),'output':str(args.output)}))

if __name__=='__main__':main()
