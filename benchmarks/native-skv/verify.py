#!/usr/bin/env python3
"""Check saved public hashes/counts/medians and losses without querying raster data."""
from pathlib import Path
import csv,hashlib,json,statistics
ROOT=Path(__file__).resolve().parent
csv.field_size_limit(16<<20)
def rows(name):
    with (ROOT/name).open() as stream:return list(csv.DictReader(stream))
def main():
    provenance=json.loads((ROOT/'PROVENANCE.json').read_text())
    for item in provenance['files']:
        assert hashlib.sha256((ROOT/item['public_path']).read_bytes()).hexdigest()==item['public_sha256'],item['public_path']
    operations=rows('data/operations.csv');lanes=rows('data/lane-cells.csv');comparisons=rows('data/fixed-summary-comparisons.csv')
    assert len(operations)==588 and len(lanes)==196 and len(comparisons)==28
    assert all(float(r['speedup_vs_natural'])>1 and float(r['speedup_vs_fastest_native'])>1 for r in comparisons)
    lookup={(r['dataset'],r['case'],r['regime'],r['lane']):r for r in lanes}
    for key,summary in lookup.items():
        records=[r for r in operations if tuple(r[k] for k in ['dataset','case','regime','lane'])==key]
        assert len(records)==3
        values=[float(r['primary_ms']) for r in records]
        assert statistics.median(values)==float(summary['primary_ms_median'])
    losses=rows('data/individual-losses.csv');assert any(r['candidate_lane']=='native-skv-summary' and r['control_lane']=='natural-ee' for r in losses)
    assert len(rows('sentinel/operations.csv'))==60 and len(rows('sentinel/pairs.csv'))==30
    print(json.dumps({'passed':True,'operations':588,'lane_cells':196,'scenario_medians':28,'sentinel_operations':60,'new_queries':0}))
if __name__=='__main__':main()
