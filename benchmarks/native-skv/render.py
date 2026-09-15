#!/usr/bin/env python3
"""Render final saved-data figures. Does not query a raster or select a runtime route."""
import argparse,csv,json,hashlib,math
from pathlib import Path
import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
import numpy as np
from matplotlib.colors import TwoSlopeNorm,LinearSegmentedColormap
from matplotlib.ticker import ScalarFormatter

DATASETS=['analytical36','analytical40-1025x1031','real-age36','worldpop1']
LABELS={'analytical36':'Analytical · 36 bands','analytical40-1025x1031':'Analytical · 40 bands','real-age36':'Real age crop · 36 bands','worldpop1':'WorldPop · 1 band'}
CASES={'A':'1 polygon / 1 band','B':'8 polygons / 1 band','C':'1 polygon / all bands','D':'8 polygons / all bands'}
GREEN='#087f70';BLUE='#55769e';ORANGE='#b36a35';RED='#b0483b';INK='#243139'
plt.rcParams.update({'font.family':'DejaVu Sans','font.size':9,'axes.spines.top':False,'axes.spines.right':False,'axes.edgecolor':'#b9c2c6','axes.labelcolor':INK,'text.color':INK,'xtick.color':INK,'ytick.color':INK,'svg.fonttype':'none','savefig.facecolor':'white'})
def rows(p):
 with p.open() as f:
  csv.field_size_limit(16<<20);return list(csv.DictReader(f))
def sha(p):return hashlib.sha256(p.read_bytes()).hexdigest()
def main():
 parser=argparse.ArgumentParser();parser.add_argument('--root',type=Path,required=True);parser.add_argument('--output',type=Path,required=True);parser.add_argument('--analysis',type=Path);a=parser.parse_args();a.output.mkdir(exist_ok=False);analysis=a.analysis or a.root/'data'
 cs=rows(analysis/'fixed-summary-comparisons.csv');ls=rows(analysis/'lane-cells.csv');sen=rows(a.root/'sentinel/cells.csv')
 lookup={(x['dataset'],x['case'],x['regime']):x for x in cs}; lanes={(x['dataset'],x['case'],x['regime'],x['lane']):x for x in ls};keys=[(d,c) for d in DATASETS for c in ('AB' if d=='worldpop1' else 'ABCD')];assert len(cs)==28 and len(ls)==196 and len(sen)==10
 saved=[]
 def save(fig,name):
  for ext in ['png','svg']:
   p=a.output/(name+'.'+ext);fig.savefig(p,dpi=170);saved.append({'path':p.name,'sha256':sha(p),'bytes':p.stat().st_size})
  plt.close(fig)
 fig,axes=plt.subplots(1,2,figsize=(12.4,9.7),sharey=True)
 for ax,regime in zip(axes,['local','http0']):
  for i,k in enumerate(keys):
   r=lookup[k+(regime,)];choices=[('native-skv-summary',GREEN,-.2), (r['fastest_native_lane'],BLUE,0),('natural-ee',ORANGE,.2)]
   if i%2==0:ax.axhspan(i-.48,i+.48,color='#f4f6f7',zorder=0)
   for lane,col,off in choices:
    l=lanes[k+(regime,lane)];v,lo,hi=(float(l['primary_ms_'+stat]) for stat in ['median','min','max'])
    ax.errorbar(v,i+off,xerr=[[v-lo],[hi-v]],fmt='o',markersize=4.6,color=col,elinewidth=1,capsize=2)
  ax.set_xscale('log');ax.set_ylim(len(keys)-.35,-.65);ax.set_title('Local source' if regime=='local' else 'Controlled HTTP · 0 ms added',loc='left',fontweight='bold',pad=13);ax.set_xlabel('Complete consumed query latency (ms, log scale)');ax.grid(axis='x',alpha=.18);ax.xaxis.set_major_formatter(ScalarFormatter());ax.set_yticks(range(len(keys)))
 axes[0].set_yticklabels([LABELS[d]+'\n'+CASES[c] for d,c in keys],fontsize=8)
 fig.suptitle('Native Skarve + SKV: final complete-query comparison',x=.025,y=.985,ha='left',fontsize=17,fontweight='bold')
 fig.text(.025,.945,'Fixed summary-enabled SKV versus the fastest of four frozen native controls and natural exactextract',fontsize=10)
 handles=[plt.Line2D([],[],marker='o',color=c,lw=0,label=n) for c,n in [(GREEN,'Native SKV + summaries'),(BLUE,'Fastest predeclared native control'),(ORANGE,'Natural exactextract (different policy)')]]
 fig.legend(handles=handles,loc='lower left',bbox_to_anchor=(.02,.035),ncol=3,frameon=False,fontsize=9)
 fig.text(.025,.014,'d15b0bc · 3 fresh process/source repetitions per cell · points = median; whiskers = full observed range · OS cache uncontrolled',fontsize=8,color='#5e6b70')
 fig.subplots_adjust(left=.245,right=.98,top=.89,bottom=.12,wspace=.1);save(fig,'01-complete-latency')
 values=np.array([[float(lookup[k+(r,)][v]) for v in ['speedup_vs_fastest_native','speedup_vs_natural'] for r in ['local','http0']] for k in keys]);logs=np.log2(values);bound=max(1,float(np.max(np.abs(logs))))
 fig,ax=plt.subplots(figsize=(10.8,8.5));cmap=LinearSegmentedColormap.from_list('winloss',[RED,'#faf8f2',GREEN]);ax.imshow(logs,cmap=cmap,norm=TwoSlopeNorm(vmin=-bound,vcenter=0,vmax=bound),aspect='auto')
 for i in range(14):
  for j in range(4):ax.text(j,i,f'{values[i,j]:.2f}×',ha='center',va='center',fontsize=10,fontweight='bold',color='white' if abs(logs[i,j])>bound*.55 else INK)
 ax.set_xticks(range(4),['Native control\nLocal','Native control\nHTTP','Natural exactextract\nLocal','Natural exactextract\nHTTP']);ax.xaxis.tick_top();ax.tick_params(axis='x',length=0,pad=10);ax.set_yticks(range(14),[LABELS[d]+' | '+c for d,c in keys],fontsize=9);ax.tick_params(axis='y',length=0);ax.axvline(1.5,color='white',linewidth=5)
 for x in [3.5,7.5,11.5]:ax.axhline(x,color='white',linewidth=3)
 fig.suptitle('How much faster is native SKV?',x=.025,y=.98,ha='left',fontsize=17,fontweight='bold');fig.text(.025,.928,'Control latency ÷ fixed SKV latency. Above 1× favors SKV; below 1× is a retained loss.',fontsize=10)
 fig.text(.025,.065,'A: 1 polygon / 1 band    B: 8 polygons / 1 band    C: 1 polygon / all bands    D: 8 polygons / all bands',fontsize=8.5)
 fig.text(.025,.032,'Native controls share the native numerical contract. Natural exactextract uses its own policy; speed does not imply identical answers.',fontsize=8)
 fig.subplots_adjust(left=.3,right=.97,top=.83,bottom=.11);save(fig,'02-speedup-matrix')
 fig,ax=plt.subplots(figsize=(11.8,7.6));snames={'g40-band-singlewide':'40 bands · band payload · HTTP +5 ms','g40-row-singlewide':'40 bands · row payload · HTTP +5 ms','real36-band-singlewide':'Real 36 bands · HTTP +5 ms','g40-band-mixed':'8 polygons · band payload · HTTP +0 ms','g40-row-mixed':'8 polygons · row payload · HTTP +0 ms'}
 for i,r in enumerate(sen):
  if i%2==0:ax.axhspan(i-.5,i+.5,color='#f4f6f7',zorder=0)
  for role,col,off in [('control',BLUE,-.12),('candidate',GREEN,.12)]:
   v,lo,hi=(float(r[role+'_'+s+'_ms']) for s in ['median','min','max']);ax.errorbar(v,i+off,xerr=[[v-lo],[hi-v]],fmt='o',color=col,markersize=5,capsize=3)
  pct=100*(float(r['candidate_over_control'])-1);ax.text(1.025,i,f'{pct:+.1f}%',transform=ax.get_yaxis_transform(),va='center',color=RED if pct>0 else GREEN,fontweight='bold')
 ax.set_yticks(range(10),[snames[r['cell_id']]+'\n'+r['state'].capitalize() for r in sen],fontsize=8.5);ax.set_ylim(9.6,-.6);ax.set_xscale('log');ax.xaxis.set_major_formatter(ScalarFormatter());ax.grid(axis='x',alpha=.18);ax.set_xlabel('Complete query latency (ms, log scale)')
 fig.suptitle('Coalescing gains and whole-build batch regressions',x=.025,y=.98,ha='left',fontsize=17,fontweight='bold');fig.text(.025,.93,'Old b7 build versus frozen d15 build · 3 exact matched pairs per row · every individual loss retained',fontsize=10)
 handles=[plt.Line2D([],[],marker='o',color=c,lw=0,label=n) for c,n in [(BLUE,'Old b7 runtime'),(GREEN,'Frozen d15 runtime')]];fig.legend(handles=handles,loc='lower left',bbox_to_anchor=(.28,.04),ncol=2,frameon=False)
 fig.text(.025,.018,'Controlled loopback · 64 MiB/s · unchanged objects/caches/caps · whiskers = observed range · right label = change in median latency',fontsize=8)
 fig.subplots_adjust(left=.32,right=.88,top=.875,bottom=.15);save(fig,'03-coalescing-regressions')
 manifest={'schema':'skarve_saved_final_charts_v1','source_sha256':{str(p.relative_to(a.root)):sha(p) for p in [analysis/'fixed-summary-comparisons.csv',analysis/'lane-cells.csv',a.root/'sentinel/cells.csv']},'script_sha256':sha(Path(__file__)),'matplotlib_version':matplotlib.__version__,'charts':saved,'no_queries':True,'statistics':'Three matched repetitions. Median and observed min/max; no confidence-interval or universal superiority claim.'}
 (a.output/'manifest.json').write_text(json.dumps(manifest,indent=2)+'\n');print(json.dumps({'figures':len(saved)//2,'output':str(a.output)}))
if __name__=='__main__':main()
