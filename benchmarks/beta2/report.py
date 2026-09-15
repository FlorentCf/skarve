#!/usr/bin/env python3
"""Static SVG/PNG publication pack from retained beta2 evidence; no benchmarks."""
from __future__ import annotations
import argparse
from collections import Counter, defaultdict
import csv
import gzip
import hashlib
import html
import io
import json
import math
from pathlib import Path
import re
import statistics
import subprocess
import textwrap

from common import digest, read_json, write_json as plain_write_json


def write_json(path, value):
    """Keep every raw field while avoiding huge whitespace-only decoded data."""
    path=Path(path)
    if path.suffix!='.gz':return plain_write_json(path,value)
    path.parent.mkdir(parents=True,exist_ok=True)
    with path.open('wb') as stream:
        with gzip.GzipFile(filename='',mode='wb',fileobj=stream,mtime=0) as compressed:
            with io.TextIOWrapper(compressed,encoding='utf-8') as text:
                json.dump(value,text,ensure_ascii=False,allow_nan=False,separators=(',',':'))

COLORS={'native':'#007f79','ee':'#346fa5','upstream':'#bc7a20','isolated':'#836093','auto':'#647785'}
LABELS={'native-fixed':'Skarve native · fixed','native-layout':'Skarve native · source layout',
        'native-prepared':'Skarve native · prepared','ee-feature-sequential':'Skarve + EE · feature',
        'ee-raster-sequential':'Skarve + EE · raster','upstream-feature-sequential':'exactextract · feature',
        'upstream-raster-sequential':'exactextract · raster','auto-ee':'Auto · EE policy only',
        'auto-both':'Auto · both policies accepted','isolated-feature-sequential':'Isolated adapter · feature',
        'isolated-raster-sequential':'Isolated adapter · raster'}
for method,label in list(LABELS.items()):
    LABELS[method+':full']=label+' · full / 5'
    fields='6' if method.startswith('native') or method=='auto-both' else '5'
    LABELS[method+':numeric']=label+(' · natural / 5' if method.startswith('upstream') else ' · numeric / '+fields)


def percentile(values,p):
    values=sorted(values);position=(len(values)-1)*p;low=math.floor(position);high=math.ceil(position)
    return values[low]+(values[high]-values[low])*(position-low)


def color(method):
    if method.startswith('ee-feature'):return '#75a5cc'
    if method.startswith('upstream-feature'):return '#cca34e'
    return COLORS[next((key for key in COLORS if method.startswith(key)),'auto')]


def scrub(value, prefixes, redactions):
    if isinstance(value,dict):return {k:scrub(v,prefixes,redactions) for k,v in value.items()}
    if isinstance(value,list):return [scrub(v,prefixes,redactions) for v in value]
    if isinstance(value,str):
        clean=value
        for prefix,label in prefixes:clean=clean.replace(prefix,label)
        clean=re.sub(r'/home/[^\s"\x27]+','<local-runtime-path>',clean)
        if clean!=value:redactions.append('local runtime/source path')
        return clean
    return value


def numeric_digest(value):
    leaves=[]
    def visit(x):
        if isinstance(x,dict):
            for k in sorted(x):visit(x[k])
        elif isinstance(x,list):
            for item in x:visit(item)
        elif x is None or isinstance(x,(int,float,bool)):leaves.append(x)
    visit(value)
    return hashlib.sha256(json.dumps(leaves,allow_nan=False,separators=(',',':')).encode()).hexdigest(),len(leaves)


def evaluation_gates(data):
    """Separate invalid controls without upgrading the retained aggregate gate."""
    tasks={r['task']['id']:r['task'] for r in data['records']}
    expected=Counter(t['id'] for t in data['freeze']['tasks'])
    membership=Counter(r['task']['id'] for r in data['records'])==expected
    invalid={f['task'] for f in data['failures'] if 'task' in f}
    invalid.update(r['task']['id'] for r in data['records'] if not r.get('passed_execution')
                   or not r.get('validation',{}).get('shape_complete'))
    invalid.update(p['task'] for p in data['upstream_pairs'] if not p['passed'])
    invalid.update(p['task'] for p in data['independent_single_pairs']['pairs'] if not p['passed'])
    invalid.update(r['task']['id'] for r in data['records']
                   if r.get('validation',{}).get('selected_backends')==['native']
                   and r['validation']['strict_differences'])
    historical={key for key,task in tasks.items() if task['method'].startswith('isolated-')}
    current_ok=(membership and data['all_frozen_inputs_unchanged'] and not (invalid-historical)
                and all('task' in f for f in data['failures']))
    return {'aggregate_passed':data['passed'],'current_product_and_natural_controls_passed':bool(current_ok),
            'invalid_task_ids':sorted(invalid),'historical_invalid_tasks':len(invalid & historical),
            'historical_mismatching_fields':sum(len(p.get('differences',[])) for p in data['upstream_pairs']
                                               if p['task'] in historical and not p['passed']),
            'valid_tasks':len(tasks)-len(invalid),'task_membership_complete':membership}


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--input',type=Path,required=True,help='programme.json')
    p.add_argument('--calibration',type=Path)
    p.add_argument('--cpp-control',type=Path)
    p.add_argument('--failure-cost',type=Path,help='Separately labelled installed rejection/cancellation supplement')
    p.add_argument('--history',type=Path,action='append',default=[],help='Retained earlier programme receipt; repeatable')
    p.add_argument('--historical-report-link',default='../../report/index.html',
                   help='Relative link from benchmarks/beta2/report to the retained beta1 report')
    p.add_argument('--output',type=Path,required=True)
    args=p.parse_args();assert not args.output.exists();args.output.mkdir(parents=True)
    raw=read_json(args.input);cal=read_json(args.calibration) if args.calibration else None
    cpp=read_json(args.cpp_control) if args.cpp_control else None
    failure_cost=read_json(args.failure_cost) if args.failure_cost else None
    if cal:assert cal['library_sha256']==raw['freeze']['library_sha256'], 'Calibration library mismatch'
    if cpp:assert cpp['native_library_sha256']==raw['freeze']['library_sha256'], 'C++ evidence library mismatch'
    if failure_cost:assert failure_cost['library_sha256']==raw['freeze']['library_sha256'], 'Cancellation library mismatch'
    redactions=[];prefixes=[(str(args.input.parent.resolve()),'<generated-inputs>'),
                          (str(Path(__file__).resolve().parents[2]),'<source-checkout>')]
    data=scrub(raw,prefixes,redactions)
    assert numeric_digest(raw)==numeric_digest(data), 'Public path sanitization changed numerical evidence'
    write_json(args.output/'data/programme.json.gz',data)
    if cal:write_json(args.output/'data/calibration.json',scrub(cal,prefixes,redactions))
    if cpp:write_json(args.output/'data/cpp-control.json.gz',scrub(cpp,prefixes,redactions))
    if failure_cost:
        clean=scrub(failure_cost,prefixes,redactions)
        assert numeric_digest(failure_cost)==numeric_digest(clean)
        write_json(args.output/'data/failure-cancellation.json',clean)
        note=Path(__file__).with_name('FAILURE_CANCELLATION.md')
        (args.output/'FAILURE_CANCELLATION.md').write_text(note.read_text().replace('failure-cancellation.json','data/failure-cancellation.json'))
    history=[]
    for index,path in enumerate(args.history):
        prior=read_json(path);clean=scrub(prior,prefixes,redactions)
        assert numeric_digest(prior)==numeric_digest(clean)
        name=f'data/history-{index+1:02d}.json.gz';write_json(args.output/name,clean)
        cpp_history=prior.get('schema')=='skarve_beta2_cpp_control_v1'
        history.append({'path':name,'input_sha256':digest(path),'passed':prior.get('passed'),
                        'kind':'C++ control' if cpp_history else 'programme',
                        'tasks':len(prior.get('records',[]))+(len(prior.get('errors',[])) if cpp_history else 0),
                        'failures':prior.get('failures',prior.get('errors',[])),
                        'numeric_leaf_digest':numeric_digest(prior)})
    reference=Path(__file__).with_name('reference-validity.json')
    write_json(args.output/'data/reference-validity.json',read_json(reference))
    (args.output/'REFERENCE_VALIDITY.md').write_text(Path(__file__).with_name('REFERENCE_VALIDITY.md').read_text())
    write_json(args.output/'data/reference-names.json',read_json(Path(__file__).with_name('reference-names.json')))
    (args.output/'ISOLATED_CONTROL_FINDING.md').write_text(Path(__file__).with_name('ISOLATED_CONTROL_FINDING.md').read_text())
    import matplotlib
    matplotlib.use('Agg')
    import matplotlib.pyplot as plt
    import numpy as np
    plt.rcParams.update({'font.family':'DejaVu Sans','font.size':10,'text.color':'#24394a',
        'axes.labelcolor':'#24394a','axes.edgecolor':'#bac7ce','axes.spines.top':False,
        'axes.spines.right':False,'figure.facecolor':'#f7f9fa','axes.facecolor':'#ffffff',
        'svg.fonttype':'none','axes.titleweight':'bold','savefig.facecolor':'#f7f9fa'})
    rows=[];figures=[]
    rounds=1 if data['freeze']['smoke'] else 3
    single_count=12 if data['freeze']['smoke'] else 100
    prep_count=8 if data['freeze']['smoke'] else 32
    footer={
        '01-singles':f'{single_count} distinct polygons per state · five fields · median / empirical p95 · one host; OS cache uncontrolled.\nNative strict and upstream fractional policies differ. Independent panel scales and speed order.',
        '02-batches':f'8'+('' if data['freeze']['smoke'] else '/64')+f' zones × '+('4' if data['freeze']['smoke'] else '24')+f' dates or six two-band files · two calls per process · {rounds}-round median; min–max is observed spread.\nFull/5 is the matched request; numeric/6 is supplemental. Policies and panel scales differ; OS cache uncontrolled.',
        '03-preparation':f'{prep_count} ordered polygons × one band · build/open + queries · {rounds}-round median; shade is observed min–max.\nNative strict and upstream fractional policies differ. Measured cohort only; no extrapolated payoff.',
        '04-remote-latency':f'Seven polygons × one band · {rounds}-round medians · same-host loopback tiled TIFF; no WAN or cold-storage claim.\nNative strict and upstream fractional policies differ. Identical frozen query order; OS cache uncontrolled.',
        '05-remote-traffic':f'Seven-query HTTP totals and median whole-process peak RSS across {rounds} rounds · same-host loopback; separate panels/units.\nGET/HEAD/body counters are observed; RSS includes imports and readers. Policies differ; OS cache uncontrolled.',
        '06-cpp-control':f'{8 if data["freeze"]["smoke"] else 64} zones × '+('4' if data['freeze']['smoke'] else '24')+f' or 12 normalized bands · same built objects · {2*rounds} calls per mode; min–max is observed spread.\nShared EE policy; copies/ownership differ. Snapshot setup and installed file/binding costs excluded; independent panel scales.',
        '07-calibration':f'{len({r["fixture"] for r in cal["rows"]}) if cal else 0} fixture classes / {len(cal["rows"]) if cal else 0} strategy rows · five fields · exact Fraction and pinned upstream controls.\nEmpirical tolerance agreement is not universal equivalence. Native strict and upstream fractional policies remain distinct.'}
    def save(fig,name,title,caption):
        if name in footer:
            note='\n'.join(textwrap.fill(line,width=max(80,int(fig.get_figwidth()*12))) for line in footer[name].split('\n'))
            fig.text(.035,-.04,note,fontsize=8.5,ha='left',va='top',color='#405564')
        fig.savefig(args.output/(name+'.svg'),bbox_inches='tight')
        svg=args.output/(name+'.svg')
        svg.write_text(svg.read_text().replace("font-family: 'DejaVu Sans'",
                                              "font-family: 'DejaVu Sans', Arial, sans-serif"))
        fig.savefig(args.output/(name+'.png'),dpi=180,bbox_inches='tight');plt.close(fig)
        figures.append({'id':name,'title':title,'caption':caption,
                        'svg_sha256':digest(args.output/(name+'.svg')),'png_sha256':digest(args.output/(name+'.png'))})
    def bars(ax,groups,figure,**dimensions):
        ordered=sorted(groups,key=lambda key:statistics.median(groups[key]))
        medians=[statistics.median(groups[key]) for key in ordered]
        ax.barh([LABELS.get(key,key) for key in ordered],medians,color=[color(key) for key in ordered],height=.62)
        ax.invert_yaxis();ax.set_xlim(left=0);ax.margins(x=.24);ax.set_xlabel('Milliseconds · lower is faster')
        extent=max((max(values) for values in groups.values()),default=0)
        ax.set_xlim(0,max(extent*1.3,1e-6))
        ax.grid(axis='x',alpha=.14);ax.set_axisbelow(True)
        for i,(key,value) in enumerate(zip(ordered,medians)):
            minimum,maximum=min(groups[key]),max(groups[key])
            ax.plot([minimum,maximum],[i,i],color='#24394a',lw=1.2)
            ax.annotate(f'{value:,.3f}',(maximum,i),xytext=(5,0),textcoords='offset points',va='center',fontsize=9)
            rows.append({'figure':figure,**dimensions,'method':key,'metric':'median_ms','value':value,
                         'min_ms':minimum,'max_ms':maximum,'n':len(groups[key])})
    gates=evaluation_gates(data)
    executed=[r for r in data['records'] if r['passed_execution'] and r.get('validation',{}).get('shape_complete')]
    complete=[r for r in executed if r['task']['id'] not in gates['invalid_task_ids']]
    complete_gates=data['passed'] and cal is not None and cal.get('passed') and cpp is not None and cpp.get('passed')
    current_gates=gates['current_product_and_natural_controls_passed'] and cal is not None and cal.get('passed') and cpp is not None and cpp.get('passed')
    if failure_cost:
        current_gates=current_gates and failure_cost.get('passed')
        complete_gates=complete_gates and failure_cost.get('passed')
    status=('Complete finite evaluation' if complete_gates else
            f'Product gates pass · {gates["historical_invalid_tasks"]} historical-control failures retained' if current_gates else
            'INCOMPLETE / FAILED GATES — inspect missing or failed evidence')

    singles=[r for r in complete if r['task']['family']=='single']
    if singles:
        fig,axes=plt.subplots(1,3,figsize=(17,6.2));fig.suptitle('Retained sources, distinct polygons',x=.035,ha='left',fontsize=21,fontweight='bold')
        for ax,state in zip(axes,('default','warm','overflow')):
            group={r['task']['method']:[c['elapsed_ms'] for c in r['calls']] for r in singles if r['task']['state']==state}
            ordered=sorted(group,key=lambda key:statistics.median(group[key]));values=[statistics.median(group[key]) for key in ordered]
            ax.barh([LABELS[k] for k in ordered],values,color=[color(k) for k in ordered],height=.58)
            ax.scatter([percentile(group[k],.95) for k in ordered],range(len(ordered)),marker='|',s=180,color='#24394a',label='Empirical p95')
            ax.invert_yaxis();ax.set_xlim(left=0);ax.margins(x=.23);ax.grid(axis='x',alpha=.14);ax.set_axisbelow(True)
            ax.set_title({'default':'No native decoded cache','warm':'1 MiB cache · prior distinct fill','overflow':'1 MiB cache · four-tile scatter'}[state],loc='left',fontsize=11)
            ax.set_xlabel('Milliseconds · bar median / mark p95')
            for i,key in enumerate(ordered):
                n=len(group[key]);v=values[i];tail=percentile(group[key],.95)
                ax.annotate(f'{v:.3f} / {tail:.3f}',(max(v,tail),i),xytext=(5,0),textcoords='offset points',va='center',fontsize=8.5)
                rows.append({'figure':'01-singles','state':state,'method':key,'metric':'median_ms','value':v,'p95_ms':tail,'n':n})
        fig.tight_layout(rect=(0,.025,1,.91))
        save(fig,'01-singles','Retained sources, distinct polygons','Complete query plus canonical five-field sink. One process per method/state; p95 is over distinct geometry IDs. Panels use independent scales and speed order; cross-panel bar lengths are not comparable. Source registration and a separate distinct cache-fill query are charged in raw lifecycle data. OS cache is uncontrolled; reference GDAL cache remains enabled.')

    batches=[r for r in complete if r['task']['family'] in ('dates','files') and r['task'].get('output_mode') in ('numeric','full')]
    if batches:
        sizes=sorted({r['task']['zones'] for r in batches})
        fig,axes=plt.subplots(len(sizes),2,figsize=(18,6.5*len(sizes)),squeeze=False)
        fig.suptitle('Complete source-owned batch lifecycles',x=.035,ha='left',fontsize=21,fontweight='bold')
        for i,size in enumerate(sizes):
            for j,family in enumerate(('dates','files')):
                groups=defaultdict(list)
                for r in batches:
                    t=r['task']
                    if t['zones']==size and t['family']==family:groups[t['method']+':'+t['output_mode']].append(r['lifecycle_ms'])
                bars(axes[i,j],groups,'02-batches',family=family,zones=size)
                axes[i,j].set_title(f'{size} zones × '+('24-date stack' if family=='dates' and not data['freeze']['smoke'] else 'date stack' if family=='dates' else 'six two-band files'),loc='left')
        fig.tight_layout(rect=(0,.015,1,.95))
        save(fig,'02-batches','Complete source-owned batch lifecycles','Open/register, first all-zone call, one repeated aligned call, output normalization/sink, close. Full / 5 lanes request the same five statistics; rich metadata differs and is charged. Native numeric / 6 and native-selected auto also compute/return integer count as supplemental competent controls; projection follows the complete response. Natural and delegated numeric lanes request five. Bars are three-round medians with observed min–max marks (smoke has one), not confidence intervals. Panels use independent scales and speed order. Historical isolation has its own lifecycle table.')

    prepared=[r for r in complete if r['task']['family']=='preparation']
    if prepared:
        curves=defaultdict(list)
        for r in prepared:
            initial=r['setup_ms']+r.get('build_ms',0)+r.get('index_registration_ms',0)
            curves[r['task']['method']].append(np.cumsum([initial]+[c['elapsed_ms'] for c in r['calls']]))
        fig,ax=plt.subplots(figsize=(11,5.4));fig.suptitle('Preparation must repay its setup cost',x=.055,ha='left',fontsize=21,fontweight='bold')
        for method,values in sorted(curves.items()):
            matrix=np.asarray(values);median=np.median(matrix,axis=0);x=np.arange(len(median))
            ax.plot(x,median,lw=2.2,color=color(method),label=LABELS[method],linestyle='--' if method=='native-prepared' else '-')
            ax.fill_between(x,matrix.min(axis=0),matrix.max(axis=0),color=color(method),alpha=.1)
            for n,v in enumerate(median):rows.append({'figure':'03-preparation','method':method,'metric':'cumulative_ms','queries':n,'value':float(v),'n':len(values)})
        ax.set_ylim(bottom=0);ax.set_xlim(left=0);ax.set_xlabel('Completed distinct polygons, in the frozen order');ax.set_ylabel('Cumulative milliseconds, including build/open')
        ax.legend(frameon=False,loc='upper left');ax.grid(alpha=.15);fig.tight_layout(rect=(0,.02,1,.88))
        if 'native-prepared' in curves and 'native-layout' in curves:
            if np.all(np.median(curves['native-prepared'],axis=0)>np.median(curves['native-layout'],axis=0)):
                footer['03-preparation']+=f' No preparation payoff within {prep_count} polygons.'
        save(fig,'03-preparation','Preparation must repay its setup cost','Observed cumulative costs for the same large-interior polygon cohort. Native preparation retains original boundary values; index bytes and build/open costs are recorded. The shaded range is the observed three-round spread. A crossing is measured only within this cohort; no 1,000-query extrapolation is presented as a result.')

    remotes=[r for r in complete if r['task']['family']=='remote']
    if remotes:
        by=defaultdict(list)
        for r in remotes:
            for call in r['calls']:by[(call['query'],r['task']['method'])].append(call)
        queries=sorted({key[0] for key in by});methods=sorted({key[1] for key in by})
        fig,ax=plt.subplots(figsize=(12,5.4));fig.suptitle('Loopback tiled TIFF: nearby, scattered and revisit queries',x=.05,ha='left',fontsize=20,fontweight='bold')
        width=.75/len(methods)
        for i,method in enumerate(methods):
            vals=[statistics.median(c['elapsed_ms'] for c in by[(q,method)]) for q in queries]
            ax.bar(np.arange(len(queries))+(i-(len(methods)-1)/2)*width,vals,width=width,color=color(method),label=LABELS[method])
            for q,v in zip(queries,vals):rows.append({'figure':'04-remote-latency','method':method,'query':q,'metric':'median_ms','value':v,'n':len(by[(q,method)])})
        states=[by[(q,methods[0])][0]['state'] for q in queries]
        ax.set_xticks(range(len(queries)),[f'{q}\n{state}' for q,state in zip(queries,states)]);ax.set_ylim(bottom=0);ax.set_ylabel('Milliseconds per complete query + sink');ax.legend(frameon=False)
        ax.grid(axis='y',alpha=.15);ax.set_axisbelow(True);fig.tight_layout(rect=(0,.02,1,.89))
        save(fig,'04-remote-latency','Loopback tiled TIFF: nearby, scattered and revisit queries','Fresh source session per round, stable conditional validators and 1 MiB native decoded cache. The coordinate sequence is labelled in advance; raw cache counters establish actual hits and evictions. Original encoded bytes travel through the same bounded source interface. The server runs on the same host with no injected network delay; these are not WAN/cloud benchmarks. Mutation rejection is retained.')
        fig,axes=plt.subplots(1,3,figsize=(17,5.4));fig.suptitle('Source traffic and process memory remain part of execution',x=.04,ha='left',fontsize=20,fontweight='bold')
        for method in methods:
            total_bytes=[];heads=[];gets=[]
            for r in remotes:
                if r['task']['method']!=method:continue
                traffic=[request for c in r['calls'] for request in c['server']]
                total_bytes.append(sum(x['body_bytes'] for x in traffic));heads.append(sum(x['method']=='HEAD' for x in traffic));gets.append(sum(x['method']=='GET' for x in traffic))
            byte=statistics.median(total_bytes);head=statistics.median(heads);get=statistics.median(gets)
            axes[0].barh(LABELS[method],byte/(1<<20),color=color(method))
            axes[1].barh(LABELS[method],get,color=color(method));axes[1].barh(LABELS[method],head,left=get,color='#a8bac3')
            peaks=[r['peak_process_rss_bytes'] for r in remotes if r['task']['method']==method]
            peak=statistics.median(peaks);axes[2].barh(LABELS[method],peak/(1<<20),color=color(method))
            rows.append({'figure':'05-remote-traffic','method':method,'metric':'body_bytes','value':byte,'head_requests':head,'get_requests':get,'median_process_peak_rss_bytes':peak,'n':len(total_bytes)})
        axes[0].set_xlabel('Body MiB for the seven measured queries');axes[1].set_xlabel('Requests · colored GET / gray HEAD');axes[2].set_xlabel('MiB · median of process peak RSS')
        for ax in axes:ax.set_xlim(left=0);ax.grid(axis='x',alpha=.15);ax.set_axisbelow(True)
        fig.tight_layout(rect=(0,.02,1,.89))
        save(fig,'05-remote-traffic','Source traffic and process memory remain part of execution','Server-observed body bytes and HTTP method counts for measured queries. Source registration, guard-failure probes and server setup are separate raw records; no HEAD/body double counting. Peak RSS covers each complete worker, including interpreter, imports and loopback server; bars show the median of three independent process peaks, not synchronized memory or a hard embedded limit. A cache can eliminate body transfers while identity requests remain.')

    if cpp:
        fig,axes=plt.subplots(1,2,figsize=(13,5.4));fig.suptitle('Same-build C++ diagnostic: source interpretation already prepared',x=.035,ha='left',fontsize=19,fontweight='bold')
        for ax,family in zip(axes,('dates','files')):
            groups=defaultdict(list)
            for record in cpp['records']:
                if record['family']==family:
                    key=record['mode']+' · '+('feature' if record['strategy']==0 else 'raster')
                    groups[key].extend(run['complete_ns']/1e6 for run in record['result']['runs'])
            bars(ax,groups,'06-cpp-control',family=family)
            bands=next(job['bands'] for job in cpp['jobs'] if job['family']==family)
            ax.set_title(f'{bands} normalized bands · '+('date source' if family=='dates' else 'six files'),loc='left')
        fig.tight_layout(rect=(0,.02,1,.86))
        if not cpp.get('passed'):
            fig.text(.035,-.015,'INCOMPLETE CONTROL: natural upstream execution failed; no matched comparison is claimed.',fontsize=10,color='#a54e52',fontweight='bold')
        save(fig,'06-cpp-control','Same-build C++ diagnostic','Independent upstream ArraySource/MapWriter versus leased-window bridge. Both receive identical pre-normalized arrays/WKB. Bars pool first and repeated calls (two per process); marks are observed min/max across those calls, not independent-trial confidence intervals. Bridge callbacks copy windows; natural control views them. Complete C++ timers exclude binary snapshot load/JSON stdout, which remain separately charged. Do not divide these array timers into installed original-file calls and call the result pure bridge overhead.')

    if cal and cal.get('rows'):
        fixtures=sorted({r['fixture'] for r in cal['rows']});matrix=[]
        for fixture in fixtures:
            subset=[r for r in cal['rows'] if r['fixture']==fixture]
            bridge=all(not r['bridge_differences'] for r in subset)
            compatible=all(not r['strict_compatibility_differences'] for r in subset)
            matrix.append([1 if bridge else -1,1 if compatible else 0])
            rows.append({'figure':'07-calibration','fixture':fixture,'metric':'bridge_upstream_agreement','value':int(bridge),'upstream_within_native_tolerance':int(compatible),'strategies':len(subset)})
        fig,ax=plt.subplots(figsize=(10,6.5));fig.suptitle('Calculation policy is an explicit choice',x=.06,ha='left',fontsize=21,fontweight='bold')
        from matplotlib.colors import ListedColormap,BoundaryNorm
        ax.imshow(matrix,cmap=ListedColormap(['#ad5559','#edcf92','#a9d3ce']),norm=BoundaryNorm([-1.5,-.5,.5,1.5],3),aspect='auto')
        ax.set_xticks([0,1],['Bridge = matching upstream\nBoth strategies','Upstream within native tolerance\nAll five fields, both strategies'])
        ax.set_yticks(range(len(fixtures)),['signed-sum cancellation' if f=='cancellation' else f.replace('_',' ') for f in fixtures]);ax.tick_params(length=0,pad=10)
        for y,row in enumerate(matrix):
            for x,value in enumerate(row):ax.text(x,y,'Agreement' if value==1 else 'Policy difference' if value==0 else 'Mismatch',ha='center',va='center',fontsize=10)
        for spine in ax.spines.values():spine.set_visible(False)
        fig.tight_layout(rect=(0,.015,1,.89))
        save(fig,'07-calibration','Calculation policy is an explicit choice','Finite exact-Fraction calibration, including raw masks, scale/offset, empty support, signed cancellation and tiny positive cells. Upstream differences are not labelled an approximate center rule or a bridge bug. Strict-native default routing remains separate. The calibration does not establish universal equivalence or an upstream error bound.')

    # Family and paired five-statistic methods are fixed. The fastest natural
    # strategy is selected after measurement and disclosed as such.
    fig,axes=plt.subplots(1,2,figsize=(13.2,6.1));fig.suptitle('Skarve · optional exactextract, measured trade-offs',x=.035,ha='left',fontsize=22,fontweight='bold')
    for ax,family in zip(axes,('dates','files')):
        candidates=[r for r in rows if r['figure']=='02-batches' and r['family']==family]
        if candidates:
            maximum=max(r['zones'] for r in candidates)
            chosen=[r for r in candidates if r['zones']==maximum and r['method'] in ('native-layout:full','ee-raster-sequential:full')]
            controls=[r for r in candidates if r['zones']==maximum and r['method'].startswith('upstream-')]
            if controls:chosen.append(min(controls,key=lambda row:row['value']))
            bars(ax,{r['method']:[r['value']] for r in chosen},'summary',family=family,zones=maximum)
            ax.set_title((('4' if data['freeze']['smoke'] else '24')+'-band source' if family=='dates' else 'Six two-band sources')+f' · {maximum} polygons',loc='left')
        else:ax.axis('off');ax.text(.05,.5,'Complete evidence unavailable',transform=ax.transAxes)
    fig.text(.035,.11,'Generated data · complete two-call lifecycles · '+('one development round' if data['freeze']['smoke'] else 'three rounds')+' · one physical host',fontsize=10)
    fig.text(.035,.065,'Native strict and upstream fractional policies differ. Setup, losses and all strategies remain in the report.',fontsize=10)
    fig.text(.035,-.015,'Natural: fastest measured strategy by median within each family. Panel scales are independent.',fontsize=9)
    fig.text(.035,.025,status,fontsize=10,fontweight='bold',color='#24394a' if complete_gates else '#a54e52')
    fig.tight_layout(rect=(0,.18,1,.95))
    save(fig,'summary','Optional execution, measured trade-offs','Paired native/full and delegated raster/full request the same five statistics for the larger generated batch. The natural control is the fastest measured natural strategy by median lifecycle, selected after measurement within each family. Native compact six-field controls and other losses remain visible in the full report. Medians shown; panel scales are independent and batch figures retain spread. Numerical policies and output metadata shapes differ; full receipt, projection and sink costs are charged.')

    # Rich output and the old isolated control have their own explicitly named
    # output/lifecycle rows. Never silently combine those costs with numeric bars.
    stages=defaultdict(list)
    for record in executed:
        task=record['task']
        if task['family'] in ('dates','files'):
            stages[(task['family'],task['zones'],task['method'],task['output_mode'])].append(record)
    stage_rows=[]
    for (family,zones,method,mode),records in sorted(stages.items()):
        row={'family':family,'zones':zones,'method':method,'output_mode':mode,'rounds':len(records),
             'numerically_valid':all(r['task']['id'] not in gates['invalid_task_ids'] for r in records)}
        for field in ('setup_ms','close_ms','lifecycle_ms','process_wall_ms','startup_ms','peak_process_rss_bytes'):
            values=[r[field] for r in records if field in r]
            row[field]=statistics.median(values) if values else None
        for index,label in ((0,'first_call_ms'),(1,'retained_call_ms')):
            values=[r['calls'][index]['elapsed_ms'] for r in records if len(r['calls'])>index]
            row[label]=statistics.median(values) if values else None
        row['sink_ms']=statistics.median(sum(c['sink_ms'] for c in r['calls']) for r in records)
        stage_rows.append(row)
    for row in stage_rows:
        controls=[r for r in stage_rows if r['family']==row['family'] and r['zones']==row['zones']
                  and r['output_mode']=='numeric' and r['method'].startswith('upstream-') and r['numerically_valid']]
        best=min(controls,key=lambda r:r['lifecycle_ms']) if controls else None
        comparable=row['numerically_valid']
        row['natural_best_strategy']=best['method'] if best and comparable else None
        row['natural_best_over_method_lifecycle']=best['lifecycle_ms']/row['lifecycle_ms'] if best and comparable else None
    with (args.output/'lifecycle_data.csv').open('w',newline='') as stream:
        keys=list(stage_rows[0]) if stage_rows else ['family','zones','method','output_mode','rounds']
        writer=csv.DictWriter(stream,fieldnames=keys,lineterminator='\n');writer.writeheader();writer.writerows(stage_rows)
    stage_caption=('Median costs in milliseconds for the larger batch, including all rich and historical isolated lanes. '
        'First and retained columns include result projection and canonical sink. Setup, calls and cleanup are inside lifecycle; '
        'worker process wall additionally includes Python startup/imports, bookkeeping and receipt transfer. Sink and isolated worker '
        'startup are nested diagnostics, not costs to add again. Native full output computes/serializes its richer result before '
        'the common five-field projection. An isolated control uses its separately recorded worker/resource envelope. '
        'The last column is the ratio of medians: faster natural upstream strategy / named method; above 1 means the named method '
        'takes less time. The natural strategy is selected after measurement within the same family/size. Native numeric is a '
        'supplemental six-field control; full native/EE lanes request the same five fields, with different charged metadata shapes. '
        'INVALID rows retain measured costs only: their answers failed the control and no speed ratio is computed.')
    maximum=max((row['zones'] for row in stage_rows),default=0)
    stage_table=['| Family / output | Method | Setup | First | Retained | Two-call lifecycle | Process wall | Natural / method |',
                 '|---|---|---:|---:|---:|---:|---:|---:|']
    def shown(value):return 'unavailable' if value is None else f'{value:,.3f}'
    stage_html=[]
    for row in stage_rows:
        if row['zones']!=maximum:continue
        cells=[row['family']+' / '+row['output_mode'],('INVALID · ' if not row['numerically_valid'] else '')+LABELS.get(row['method']+':'+row['output_mode'],LABELS.get(row['method'],row['method']))]+[
            shown(row[key]) for key in ('setup_ms','first_call_ms','retained_call_ms','lifecycle_ms','process_wall_ms')]
        cells.append('not comparable' if row['natural_best_over_method_lifecycle'] is None else f"{row['natural_best_over_method_lifecycle']:.3f}×")
        stage_table.append('| '+' | '.join(cells)+' |')
        stage_html.append('<tr>'+''.join('<td>'+html.escape(cell)+'</td>' for cell in cells)+'</tr>')

    allkeys=sorted({k for r in rows for k in r})
    with (args.output/'chart_data.csv').open('w',newline='') as stream:
        writer=csv.DictWriter(stream,fieldnames=allkeys,lineterminator='\n');writer.writeheader();writer.writerows(rows)
    failures=data['failures']
    summary={'schema':'skarve_beta2_public_report_v1','complete_evidence_gates_passed':bool(complete_gates),'programme_passed':data['passed'],
        'current_product_evidence_gates_passed':bool(current_gates),'evaluation_gates':gates,
        'calibration_passed':cal.get('passed') if cal else None,'cpp_control_passed':cpp.get('passed') if cpp else None,
        'failure_cancellation_cost_passed':failure_cost.get('passed') if failure_cost else None,
        'source_commit':data['freeze']['source_commit'],
        'harness_source_commit':data['freeze'].get('harness_source_commit',data['freeze']['source_commit']),
        'installed_package_source_commit':data['freeze'].get('package_source_commit'),
        'report_checkout_commit':subprocess.run(['git','rev-parse','HEAD'],cwd=Path(__file__).resolve().parents[2],text=True,capture_output=True).stdout.strip() or None,
        'report_generator_sha256':digest(__file__),
        'native_library_sha256':data['freeze']['library_sha256'],
        'retained_beta1_report':args.historical_report_link,
        'seed':data['freeze']['seed'],'figures':figures,'chart_rows':rows,'lifecycle_rows':stage_rows,'failures':failures,'history':history,
        'inputs':{'programme_sha256':digest(args.input),'calibration_sha256':digest(args.calibration) if args.calibration else None,
                  'cpp_control_sha256':digest(args.cpp_control) if args.cpp_control else None,
                  'failure_cost_sha256':digest(args.failure_cost) if args.failure_cost else None,'renderer_sha256':digest(__file__)},
        'numeric_leaf_digest_before_after':numeric_digest(raw),'redacted_string_occurrences':len(redactions),
        'render_versions':{'matplotlib':matplotlib.__version__},'visual_qa':'pending actual rendered inspection',
        'limitations':data['limitations']}
    write_json(args.output/'summary.json',summary)
    title='Skarve Benchmark Report'
    lead=f'{status}. {len(executed)}/{len(data["records"])} task receipts have complete executed outputs; {len(complete)} passed their applicable numerical gates. The aggregate programme remains passed={data["passed"]}. Installed package source `{data["freeze"].get("package_source_commit")}`; benchmark harness source `{data["freeze"].get("harness_source_commit",data["freeze"]["source_commit"])}`; native SHA `{data["freeze"]["library_sha256"]}`; generated seed `{data["freeze"]["seed"]}`.'
    paragraphs=['Optional exactextract · beta.2',lead,'This is a finite generated-data evaluation of an optional backend in the ordinary installed source/query interface. It is not a new zonal-statistics algorithm, an application adoption study, or evidence that one backend is universally faster.',
        'Native strict and exactextract fractional calculation use different finite-precision contracts. Five shared fields are compared: sum, valid fractional support, mean, minimum and maximum. Support is not an integer count. Forced delegation, strict conflicts and unsupported hard-isolation requests are tested separately.',
        'Each timed worker is a fresh process, with retained work inside it. Interpreter/import process wall, source setup, first/repeated calls, canonical result consumption, cleanup, cache counters and peak RSS are retained. Nested processor/bridge/callback clocks must not be added. Fixture generation and independent oracles are outside timed operations.',
        'No claim of cold disk, WAN/cloud latency, another physical host, or inferred process-isolation protection is made. HTTP figures use bounded conditional loopback requests. No private application comparison is included.',
        'An empty Skarve decoded cache is not cold operating-system or storage state: the kernel page cache was not flushed. Auto with only the EE policy accepted can select EE; accepting the native policy continues to select native. These measurements do not promote cross-policy performance routing.',
        'The old isolated adapter, when supplied as a private development control, remains separately labelled and is not exported as a release dependency. It has a 1 GiB worker and 16 MiB worker GDAL cache; it is not silently treated as an equal-budget embedded route.']
    paragraphs.append('The timing harness was frozen before measurement and its source checks passed at completion. Later report-only changes classify the historical failures, compact retained JSON and add readable figure labels; the final report generator hash is recorded separately. Core, bindings, timed worker and programme code remain unchanged. Exact timing-harness revision is recoverable from Git history; replay from final source uses the same timing logic and seed in a fresh directory.')
    prior_text='Earlier failures remain separate from the final gate: '+('; '.join(f'[{h["tasks"]}-task prior receipt]({h["path"]}) ({len(h["failures"])} failed gates)' for h in history) if history else 'no earlier programme receipt supplied')+'. [Cropped NumPy reference diagnosis and correction](REFERENCE_VALIDITY.md); [small pinned reproduction output](data/reference-validity.json).'
    failure_groups=defaultdict(list)
    task_methods={r['task']['id']:r['task']['method'] for r in data['records']}
    mismatch_counts={p['task']:len(p.get('differences',[])) for p in data['upstream_pairs'] if not p['passed']}
    for failure in failures:failure_groups[task_methods.get(failure.get('task'),'unscoped failure')].append(failure)
    failure_table=['| Failed control / method | Tasks | Field mismatches |','|---|---:|---:|']
    failure_html=[]
    for method,items in sorted(failure_groups.items()):
        count=sum(mismatch_counts.get(item.get('task'),0) for item in items)
        label=LABELS.get(method,method)
        failure_table.append(f'| {label} | {len(items)} | {count:,} |')
        failure_html.append(f'<tr><td>{html.escape(label)}</td><td>{len(items)}</td><td>{count:,}</td></tr>')
    disposition=('Disposition: the optional embedded backend is supported only for its explicit upstream fractional policy and documented eligible requests. '
        'Its benefit depends on workload and strategy; the report retains both wins and losses against native and natural upstream controls. '
        'Native strict remains the default. Accepting the native policy keeps auto on native; EE-only acceptance can select EE. '
        'These results do not enable performance-driven cross-policy promotion.')
    hero=next(f for f in figures if f['id']=='summary')
    supplement=('A separate installed functional-cost probe records rejection, active cancellation and drain. '
        'Its deliberately held remote callback contributes 25 ms; cancel-to-drain is not CPU-only interrupt latency. '
        'The interrupted reader must be closed/reinfused; the session can then be reused. '
        '[Scope, measured table and commands](FAILURE_CANCELLATION.md), [raw receipt](data/failure-cancellation.json).') if failure_cost else 'No timed rejection/cancellation supplement was supplied.'
    historical=f'[Retained beta1 historical benchmark report]({args.historical_report_link}). Its inputs, runtime and lifecycle definitions remain separate; beta2 does not replace its losses.'
    isolated_note=(f'The historical isolated adapter failed {gates["historical_invalid_tasks"]} final controls '
        f'({gates["historical_mismatching_fields"]:,} mismatching field comparisons). A two-band untimed reproduction isolates unnamed RasterSource wrappers: '
        'unique names restore the expected distinct answers in both strategies. The historical adapter is unchanged, its '
        'timings are invalid controls with no ratios, and the aggregate failure is preserved. See '
        '[bounded diagnosis](ISOLATED_CONTROL_FINDING.md) and [reproduction receipt](data/reference-names.json).') if gates['historical_invalid_tasks'] else ''
    body=['# '+title,paragraphs[0],lead,f'![{hero["title"]}](summary.svg)',hero['caption'],disposition,
          '## Scope and interpretation',*paragraphs[2:],historical,
          '## Failures remain part of the result','\n'.join(failure_table) if failures else 'No programme gate failed.',
          '[Exact task IDs, differences and retained observations](data/programme.json.gz).',isolated_note,prior_text,
          '## Measured families']
    for figure in figures:
        if figure['id']!='summary':body += ['### '+figure['title'],f'![{figure["title"]}]({figure["id"]}.svg)',figure['caption']]
    body += ['## Rejection and cancellation costs',supplement,
             '## Lifecycle and output appendix',stage_caption,'\n'.join(stage_table),
             '[All lifecycle, startup, sink and memory observations](lifecycle_data.csv).']
    body += ['## Reproduce','Install the approved optional-feature artifact and benchmark dependencies. Run the finite commands below in a fresh output directory and a serial timing lane. The old private adapter is optional and is omitted from this portable command. No network dataset is fetched.',f'```sh\npython benchmarks/beta2/programme.py freeze --output scratch/beta2 --artifacts dist/beta2 --seed {data["freeze"]["seed"]}{" --smoke" if data["freeze"]["smoke"] else ""}\npython benchmarks/beta2/programme.py run --output scratch/beta2\npython tests/exactextract_contracts.py --enabled yes --scratch scratch/calibration --output scratch/calibration.json\npython benchmarks/beta2/cpp_control.py --folder scratch/beta2 --control target/exactextract-control/skarve-ee-control --output scratch/cpp --zones {8 if data["freeze"]["smoke"] else 64} --rounds {1 if data["freeze"]["smoke"] else 3} --native-build-receipt "$NATIVE_BUILD_RECEIPT"\npython benchmarks/beta2/report.py --input scratch/beta2/programme.json --calibration scratch/calibration.json --cpp-control scratch/cpp/results.json --output scratch/report\n```',
        'The C++ control executable is a separately built diagnostic; its build command is in the optional-backend native build instructions. Set NATIVE_BUILD_RECEIPT to the receipt emitted by the optional build that produced the measured library; the control verifies exact upstream archive, bridge object and GEOS hashes. Source builds and ordinary installed execution remain separate provenance modes.',
        '[Complete retained data](data/programme.json.gz), [chart CSV](chart_data.csv), [summary and hashes](summary.json).',
        'Archive replay: the timing harness enumerates tracked source files. If starting from a source tarball without Git history, first run `git init`, `git add -A`, and a local snapshot commit using your own configured author. This creates replay provenance; it does not reproduce the original commit ID. The archive SOURCE_MANIFEST and unchanged native hash retain the distributed-source identity. A normal clone already has the required Git metadata.',
        'Environment: `'+json.dumps(data['freeze']['host'],sort_keys=True)+'`. Versions: `'+json.dumps(data['freeze']['versions'],sort_keys=True)+'`.',
        '## Statistical definitions',*data['freeze']['statistical_definitions'].values()]
    (args.output/'REPORT.md').write_text('\n\n'.join(body)+'\n')
    cards='<section><h2>Measured disposition</h2><img src="summary.svg" alt="Measured complete lifecycle trade-offs"><p>'+html.escape(hero['caption'])+'</p><p>'+html.escape(disposition)+'</p><p><a href="summary.png">Summary PNG</a> · <a href="summary.svg">Summary SVG</a></p></section>'
    cards+='<section><h2>Scope and retained history</h2>'+''.join('<p>'+html.escape(x)+'</p>' for x in paragraphs[2:])+'<p><a href="'+html.escape(args.historical_report_link,quote=True)+'">Beta1 benchmark report</a>. Its inputs, runtime and lifecycle definitions remain separate; beta2 does not replace its losses.</p></section>'
    if isolated_note:
        cards+=f'<section><h2>Historical isolated control failed</h2><p>{gates["historical_invalid_tasks"]} historical controls failed: {gates["historical_mismatching_fields"]:,} field mismatches. The aggregate remains failed. Current product/native and natural-control gates are reported separately. Invalid historical costs remain in the table without speed ratios.</p><p><a href="ISOLATED_CONTROL_FINDING.md">Bounded diagnosis</a> · <a href="data/reference-names.json">Two-band reproduction receipt</a></p></section>'
    cards+=''.join(f'<section><h2>{html.escape(f["title"])}</h2><img src="{f["id"]}.svg" alt="{html.escape(f["title"])}"><p>{html.escape(f["caption"])}</p><p><a href="{f["id"]}.svg">SVG</a> · <a href="{f["id"]}.png">PNG</a></p></section>' for f in figures if f['id']!='summary')
    if failure_cost:
        cards+='<section><h2>Rejection and cancellation costs</h2><p>A separate installed probe measures pre-admission rejection and active cancel-to-drain. A controlled 25 ms callback hold dominates the latter; this is not CPU-only interrupt latency or a hard kill guarantee. The cancelled reader requires close/reinfuse; the same session then remains usable.</p><p><a href="FAILURE_CANCELLATION.md">Measured table, scope and commands</a> · <a href="data/failure-cancellation.json">Raw receipt</a></p></section>'
    cards+='<section><h2>Retained development failures</h2><p>The initial native numeric/five requests were unsupported; native six-field compact controls and exact-request full/five controls are now labelled separately. The pinned cropped masked-array reference discrepancy was corrected by changing only invalid array entries to explicit NaN nodata, with valid-bit assertions. No engine change or tolerance widening followed these findings.</p><ul>'+''.join(f'<li><a href="{h["path"]}">{h["tasks"]}-task prior receipt</a>: {len(h["failures"])} failed gates retained.</li>' for h in history)+'</ul><p><a href="REFERENCE_VALIDITY.md">Diagnosis and reproduction command</a> · <a href="data/reference-validity.json">Pinned small reproduction output</a></p></section>'
    cards+='<section><h2>Complete calls, output shapes and isolation</h2><p>'+html.escape(stage_caption)+'</p><div class="chart"><table><thead><tr>'+''.join('<th>'+x+'</th>' for x in ('Family / output','Method','Setup','First','Retained','Two-call lifecycle','Process wall','Natural / method'))+'</tr></thead><tbody>'+''.join(stage_html)+'</tbody></table></div><p><a href="lifecycle_data.csv">All lifecycle, startup, sink and memory observations</a></p></section>'
    page='<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>'+title+'</title><style>body{margin:0;background:#f2f6f7;color:#24394a;font:16px/1.6 system-ui,sans-serif;overflow-wrap:anywhere}main{max-width:1240px;padding:42px 28px;margin:auto}h1{font-size:40px;line-height:1.15;letter-spacing:-1px}h2{font-size:25px;margin-top:0}p{max-width:1000px}section{margin:34px 0;padding:28px;background:white;border-top:3px solid #007f79}img{width:100%;height:auto}a{color:#006f73}pre{white-space:pre-wrap;overflow-wrap:anywhere;font-size:13px}header{padding-bottom:24px}nav{display:flex;gap:22px;flex-wrap:wrap}@media(max-width:700px){main{padding:24px 14px}h1{font-size:32px}section{padding:16px}img{min-width:620px}.chart{overflow-x:auto}}@media print{body{background:white}main{max-width:none}section{break-inside:avoid}}</style><main><header><p>SKARVE / BETA2 / GENERATED-SOURCE EVALUATION</p><h1>'+title+'</h1>'+''.join('<p>'+html.escape(x)+'</p>' for x in paragraphs[:2])+'<nav><a href="REPORT.md">Full report and commands</a><a href="chart_data.csv">Chart CSV</a><a href="data/programme.json.gz">Retained raw results</a><a href="summary.json">Hashes and definitions</a></nav></header>'+cards+'<section><h2>Failed controls remain inspectable</h2><table><thead><tr><th>Control</th><th>Tasks</th><th>Field mismatches</th></tr></thead><tbody>'+''.join(failure_html)+'</tbody></table><p><a href="data/programme.json.gz">Exact task IDs, differences and original aggregate gate</a></p></section></main></html>'
    # Wrapper is limited to chart images so narrow screens retain readable text.
    page=re.sub(r'(<img [^>]+>)',r'<div class="chart">\1</div>',page)
    page=page.replace('</style>','table{border-collapse:collapse;font-size:13px;width:100%}th,td{padding:8px 10px;text-align:right;border-bottom:1px solid #dce4e8}td:first-child,td:nth-child(2),th:first-child,th:nth-child(2){text-align:left}th{white-space:nowrap}.chart{overflow-x:auto}</style>')
    (args.output/'index.html').write_text(page)
    print(json.dumps({'figures':len(figures),'programme_passed':data['passed'],'output':str(args.output)}))


if __name__=='__main__':main()
