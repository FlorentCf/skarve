#!/usr/bin/env python3
"""Consume an owner-verified serving profile through the installed engine.

The profile binds two equivalent source views and a measured eligibility rule.
This example does not infer equivalence from filenames or choose a raster format.
See docs/serving-profiles.md for provisioning and the identity trust boundary.
"""
import argparse
import json
from pathlib import Path
from skarve import Skarve


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--profile', type=Path, required=True)
    p.add_argument('--request', type=Path, required=True)
    p.add_argument('--view-id', required=True)
    p.add_argument('--access-class', default='unknown')
    args = p.parse_args()
    profile = json.loads(args.profile.read_text())
    request = json.loads(args.request.read_text())
    with Skarve() as engine:
        result = engine.sum_selected(profile, request, view_id=args.view_id,
            access_class=args.access_class, numerical_policy='hm_demographics_ordered_v1')
    assert result['complete']
    print(json.dumps(result, allow_nan=False))


if __name__ == '__main__':
    main()
