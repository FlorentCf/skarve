#!/usr/bin/env python3
"""Focused installed profile selection, exact outputs and invalid-contract checks."""
import argparse
import asyncio
import copy
import json
from pathlib import Path
import sys


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--fixture', type=Path, required=True)
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--library', type=Path)
    p.add_argument('--checkout-bindings', action='store_true')
    args = p.parse_args()
    if args.checkout_bindings:
        sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'bindings/python'))
    from skarve import Skarve, EngineError
    from serving_profile_fixture import build_profile
    fixture = json.loads(args.fixture.read_text())
    request = fixture['ordered_request']
    expected = fixture['ordered_reference']
    original = Path(fixture['original_for_compile'])
    compiled = args.fixture.parent / 'forty-grouped-deflate.skv'
    with Skarve(args.library) as engine:
        profile = build_profile(engine, original, compiled, args.output)
        options = {'numerical_policy': profile['numerical_policy'], 'view_id': profile['view_id']}
        selected = engine.sum_selected(profile, request, access_class='fixture-qualified-local', **options)
        direct = engine.sum_selected(profile, request, **options)
        async_result = asyncio.run(engine.sum_selected_async(profile, request,
            access_class='fixture-qualified-local', **options))
        assert selected['routing']['selected'] == async_result['routing']['selected'] == 'accelerated'
        assert direct['routing']['selected'] == 'direct'
        assert selected['rows'] == direct['rows'] == async_result['rows'] == expected
        for answer in [selected, direct, async_result]:
            assert answer['complete'] and answer['routing']['source_closed']
            assert not answer['routing']['unselected_source_opened']
            assert answer['provenance']['summaries_used'] is False
        sparse = copy.deepcopy(request)
        sparse['bands'] = [39]
        sparse_result = engine.sum_selected(profile, sparse, access_class='fixture-qualified-local', **options)
        assert sparse_result['routing']['selected'] == 'direct'
        assert sparse_result['rows'][0]['bands'] == expected[0]['bands'][:1]
        rejected = []
        tests = [('wrong-view', profile, request, {**options, 'view_id': 'another-grid'}),
                 ('wrong-policy', profile, request, {**options, 'numerical_policy': 'strict_selected_v1'}),
                 ('nodata-override', profile, {**request, 'nodata': [-9999] * 3}, options)]
        changed = copy.deepcopy(profile)
        changed['accelerated']['spec']['identity']['sha256'] = '0' * 64
        tests.append(('changed-identity', changed, request, options))
        for name, value, selection, opts in tests:
            try:
                engine.sum_selected(value, selection, access_class='fixture-qualified-local', **opts)
            except EngineError:
                rejected.append(name)
            else:
                raise AssertionError('Invalid profile contract accepted: ' + name)
        assert engine.sum_selected(profile, request, **options)['rows'] == expected
    (args.output / 'request.json').write_text(json.dumps(request, indent=2) + '\n')
    print(json.dumps({'passed': True, 'profile': str((args.output / 'profile.json').resolve()),
        'request': str((args.output / 'request.json').resolve()), 'view_id': profile['view_id'],
        'access_class': 'fixture-qualified-local', 'rows': expected, 'rejected': rejected,
        'independent_complete_equivalence': True, 'default_route': 'direct',
        'qualified_route': 'accelerated', 'sparse_route': 'direct'}, allow_nan=False))


if __name__ == '__main__':
    main()
