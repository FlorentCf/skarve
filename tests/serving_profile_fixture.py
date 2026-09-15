"""Provision a generated-only profile after an independent complete comparison.

This is test tooling, not a claim that two object hashes establish equivalence.
The consumer examples take an already provisioned profile instead.
"""
import copy
import hashlib
import json
from pathlib import Path
from skv_format_oracle import Oracle


def canonical(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':'),
                                    ensure_ascii=False, allow_nan=False).encode()).hexdigest()


def digest(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def selector(spec):
    return {key: spec.get(key, default) for key, default in [
        ('format', 'geotiff'), ('variable', None), ('overview', None), ('crs', None),
        ('longitude_shift', 0), ('bands', None)]}


def interpretation(view):
    result = copy.deepcopy(view)
    raw = result['raw_metadata']
    raw.pop('source_band_count', None)
    raw.pop('source_overview', None)
    for band in raw['bands']:
        band.pop('original_band_index', None)
    return result


def build_profile(engine, original, compiled, output):
    original, compiled, output = Path(original), Path(compiled), Path(output)
    output.mkdir(parents=True, exist_ok=False)
    candidates = {}
    for name, path, format in [('direct', original, 'geotiff'), ('accelerated', compiled, 'skv')]:
        spec = {'location': str(path.resolve()), 'format': format, 'use_summaries': False,
                'identity': {'sha256': digest(path), 'byte_length': path.stat().st_size,
                             'policy': 'verify'}}
        with engine.infuse(spec) as source:
            view = source.inspect()['serving_profile_view']
        candidates[name] = {'spec': spec, 'expected': view}
    direct_view = candidates['direct']['expected']
    semantic = canonical(interpretation(direct_view))
    assert semantic == canonical(interpretation(candidates['accelerated']['expected']))
    samples, masks = hashlib.sha256(), hashlib.sha256()
    with Oracle(compiled) as oracle:
        verification = oracle.verify_payloads()
        checked = oracle.compare_source(original)
        for index in range(len(oracle.bands)):
            raw, mask = oracle.band(index)
            samples.update(raw.tobytes())
            masks.update(mask.tobytes())
    receipt = {'passed': True, 'verifier': 'independent_skv_oracle_v1',
               'verifier_sha256': digest(Path(__file__).with_name('skv_format_oracle.py')),
               'verification': verification, 'bands': checked,
               'samples_sha256': samples.hexdigest(), 'masks_sha256': masks.hexdigest(),
               'interpretation_sha256': semantic,
               'canonical_bytes': 'Exposed band order, each complete native grid in row-major order; '
                                  'samples are original little-endian typed bytes, masks are one byte per cell.'}
    receipt_path = output / 'equivalence.json'
    receipt_path.write_text(json.dumps(receipt, indent=2) + '\n')
    attestation = {'kind': 'owner_verified_full_view_v1', 'verifier': receipt['verifier'],
                   'receipt_sha256': digest(receipt_path),
                   'checks': ['typed_sample_bits', 'mask_bytes', 'full_interpretation'],
                   'samples_sha256': samples.hexdigest(), 'masks_sha256': masks.hexdigest(),
                   'interpretation_sha256': semantic,
                   'cells_per_band': direct_view['grid']['width'] * direct_view['grid']['height']}
    for name, candidate in candidates.items():
        identity = candidate['spec']['identity']
        attestation[name] = {key: identity[key] for key in ['sha256', 'byte_length']}
        attestation[name].update(selector_sha256=canonical(selector(candidate['spec'])),
                                 expected_view_sha256=canonical(candidate['expected']))
    # Deliberately a fixture rule, with no performance recommendation attached.
    profile = {'schema': 'skarve_serving_profile_v1', 'profile_id': 'generated-routing-contract',
               'view_id': 'generated-forty-native', 'numerical_policy': 'hm_demographics_ordered_v1',
               **candidates, 'attestation': attestation,
               'accelerated_when': {'enabled': True, 'access_classes': ['fixture-qualified-local'],
                   'bands': [0, 17, 39], 'polygons': [1, 2], 'windows': [1, 4],
                   'selected_cells': [1, 2048], 'envelope_width': [1, 32],
                   'envelope_height': [1, 32], 'envelope_cells': [1, 1024]}}
    (output / 'profile.json').write_text(json.dumps(profile, indent=2) + '\n')
    return profile
