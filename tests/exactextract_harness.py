"""Small structural gates that prevent partial results becoming benchmark passes."""
from copy import deepcopy
from pathlib import Path
import sys

import pytest

sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'benchmarks/beta2'))
from common import FIELDS, NATIVE_FIELDS, NATIVE_POLICY, differences, native_bands
from programme import check_record, paired_upstream
from cpp_control import canonical
from worker import query_metrics, requested_statistics


def test_native_band_ids_checked_before_projection():
    rows=[{'band':i,**dict(zip(NATIVE_FIELDS,[1.,1.,1.,1.,1.]))} for i in (0,1)]
    assert len(native_bands({'bands':rows},[0,1]))==2
    for invalid in (rows[:1],rows[::-1],[rows[0],rows[0]]):
        with pytest.raises(AssertionError):native_bands({'bands':invalid},[0,1])


def test_missing_common_statistic_or_band_is_not_zip_truncated():
    good=dict.fromkeys(FIELDS,1.)
    assert not differences([good],[good],strict=False)
    for actual in ([],[good,good],[{'sum':1.}]):
        with pytest.raises(AssertionError):differences(actual,[good],strict=False)


def single_record():
    task={'family':'single','method':'native-layout','state':'default'}
    fixtures={'single_zones':[{'id':'z','pattern':'hot'}]}
    oracle={('single','single','z'):[dict.fromkeys(FIELDS,1.)|{'mass':1.}]}
    record={'calls':[{'query':'z','provenance':{'selected_backend':'native','numerical_policy':NATIVE_POLICY},
                      'answers':[{'zone':'z','source':'single','bands':[dict.fromkeys(FIELDS,1.)]}]}]}
    return record,task,fixtures,oracle


def test_missing_duplicate_unknown_rows_fail_membership():
    record,task,fixtures,oracle=single_record()
    assert check_record(record,task,fixtures,oracle)['shape_complete']
    for rows in ([],record['calls'][0]['answers']*2,[{'zone':'other','source':'single','bands':[dict.fromkeys(FIELDS,1.)]}]):
        bad=deepcopy(record);bad['calls'][0]['answers']=rows
        with pytest.raises(AssertionError):check_record(bad,task,fixtures,oracle)


def test_backend_provenance_is_a_gate():
    record,task,fixtures,oracle=single_record()
    record['calls'][0]['provenance']['selected_backend']='exactextract'
    with pytest.raises(AssertionError):check_record(record,task,fixtures,oracle)


def test_missing_matching_upstream_is_not_accepted():
    record={'task':{'id':'ee','family':'dates','zones':8,'round':0,'method':'ee-raster-sequential'},
            'passed_execution':True,'validation':{'selected_backends':['exactextract']},'calls':[]}
    result=paired_upstream([record])
    assert len(result)==1 and not result[0]['passed']


def test_cpp_control_missing_fields_or_invalid_defined_bytes_fail():
    good={'values':[1.]*5,'defined':[1]*5}
    assert canonical(good,1,1)==[[dict.fromkeys(FIELDS,1.)]]
    for bad in ({'values':[1.]*4,'defined':[1]*4},
                {'values':[1.]*5,'defined':[1,1,2,1,1]},
                {'values':[float('nan')]*5,'defined':[1]*5}):
        with pytest.raises(AssertionError):canonical(bad,1,1)


def test_backend_specific_cache_and_source_diagnostics_are_retained():
    value={'work':{'decoded_cache_hits':3},'source_access':{'body_bytes':1024},'bands':[]}
    assert query_metrics(value)=={'work':{'decoded_cache_hits':3},'source_access':{'body_bytes':1024}}
    assert query_metrics({'streaming':{'windows':7}})=={'streaming':{'windows':7}}


def test_native_numeric_extra_statistic_does_not_leak_into_matching_five_lane():
    assert requested_statistics('native-layout','numeric')==list(FIELDS)+['count']
    assert requested_statistics('auto-both','numeric')==list(FIELDS)+['count']
    for method in ('native-layout','ee-raster-sequential','auto-ee'):
        assert requested_statistics(method,'full')==list(FIELDS)
    assert requested_statistics('ee-raster-sequential','numeric')==list(FIELDS)


def test_cropped_normalized_reference_validity_matches_both_strategies():
    import numpy as np
    from common import array_rasters, upstream, rectangle, fractional_oracle
    values=np.arange(80,dtype=np.float64).reshape(8,10);values[3,4]=-9999.
    valid=np.ones_like(values,dtype=np.uint8);valid[3,4]=0
    normalized={'values':[values],'valid':[valid],'extent':[0.,0.,10.,8.],
                'transform':[0.,1.,0.,8.,0.,-1.],'crs':'EPSG:3857'}
    zone={'id':'crop','geometry':rectangle(2.25,1.5,7.75,6.25)}
    expected=fractional_oracle(values,valid,normalized['transform'],[(1,[2.25,1.5,7.75,6.25])])
    before=values.copy()
    for strategy in ('feature-sequential','raster-sequential'):
        actual=upstream(array_rasters(normalized),[zone],normalized['crs'],strategy)[0]['bands']
        assert not differences(actual,[expected],strict=False)
    assert np.array_equal(values.view(np.uint64),before.view(np.uint64))


def test_historical_control_failure_never_upgrades_aggregate_or_hides_product_failure():
    import importlib.util
    spec=importlib.util.spec_from_file_location('beta2_report',Path(__file__).resolve().parents[1]/'benchmarks/beta2/report.py')
    module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
    evaluation_gates=module.evaluation_gates
    tasks=[{'id':'native','method':'native-layout'},{'id':'old','method':'isolated-feature-sequential'}]
    data={'passed':False,'freeze':{'tasks':tasks},'all_frozen_inputs_unchanged':True,
          'records':[{'task':task,'passed_execution':True,'validation':{'shape_complete':True,
                      'selected_backends':['native'] if task['id']=='native' else [],'strict_differences':[]}}
                     for task in tasks],
          'failures':[{'task':'old','reason':'matching-upstream mismatch'}],
          'upstream_pairs':[{'task':'old','passed':False,'differences':[{'field':'sum'}]}],
          'independent_single_pairs':{'pairs':[]}}
    gate=evaluation_gates(data)
    assert not gate['aggregate_passed'] and gate['current_product_and_natural_controls_passed']
    assert gate['valid_tasks']==1 and gate['historical_invalid_tasks']==1
    assert gate['historical_mismatching_fields']==1
    for change in ('native','missing','duplicate','identity','unscoped'):
        bad=deepcopy(data)
        if change=='native':bad['records'][0]['validation']['strict_differences']=[{'field':'sum'}]
        elif change=='missing':bad['records'].pop(0)
        elif change=='duplicate':bad['records'].append(bad['records'][0])
        elif change=='identity':bad['all_frozen_inputs_unchanged']=False
        else:bad['failures'].append({'reason':'Unscoped failure'})
        assert not evaluation_gates(bad)['current_product_and_natural_controls_passed']
