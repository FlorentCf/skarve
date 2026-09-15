#!/usr/bin/env python3
"""Qualify the installed C ABI on a guarded 128 KiB native stack.

Requires cc only for this test harness; the installed engine needs no compiler.
The expected-crash mode is a private regression control for a pre-fix library.
"""
import argparse
import hashlib
import json
from pathlib import Path
import resource
import signal
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--library", type=Path, required=True)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--scratch", type=Path, required=True)
    parser.add_argument("--expect-crash", action="store_true")
    parser.add_argument("--predictor", choices=["none", "byte_delta_v1"], default="none")
    parser.add_argument("--payload-layout", choices=["band", "row_group_v1"], default="band")
    parser.add_argument("--overview", type=int, choices=range(64))
    parser.add_argument("--ordered", action="store_true", help="Also exercise the explicit source-owned ordered policy")
    args = parser.parse_args()
    args.scratch.mkdir(parents=True, exist_ok=False)
    executable = args.scratch / "guard"
    subprocess.run(["cc", "-O2", "-Wall", "-Wextra", "-Werror", "-std=gnu11", "-pthread",
                    str(Path(__file__).with_suffix(".c")), "-ldl", "-o", str(executable)], check=True, timeout=60)

    def bounds():
        resource.setrlimit(resource.RLIMIT_CORE, (0, 0))

    checks = []
    for codec in (["deflate"] if args.expect_crash else ["none", "deflate"]):
        output = args.scratch / (codec + ".skv")
        command = [str(executable), str(args.library.resolve()), str(args.source.resolve()), str(output), codec]
        if args.predictor != "none" or args.overview is not None or args.ordered or args.payload_layout != "band":
            command.append(args.predictor)
        if args.overview is not None or args.ordered or args.payload_layout != "band":
            command.append(str(args.overview) if args.overview is not None else "none")
        if args.ordered or args.payload_layout != "band":
            command.append("ordered" if args.ordered else "none")
        if args.payload_layout != "band":
            command.append(args.payload_layout)
        result = subprocess.run(command, text=True, capture_output=True, timeout=120, preexec_fn=bounds)
        (args.scratch / (codec + ".stdout.jsonl")).write_text(result.stdout)
        (args.scratch / (codec + ".stderr.txt")).write_text(result.stderr)
        check = {"codec": codec, "predictor": args.predictor, "payload_layout": args.payload_layout,
                 "exit_code": result.returncode, "command": command}
        checks.append(check)
        if args.expect_crash:
            assert result.returncode in (-signal.SIGSEGV, -signal.SIGABRT), check
        else:
            assert result.returncode == 0, (check, result.stderr)
            replies = [json.loads(line) for line in result.stdout.splitlines()]
            assert len(replies) == (8 if args.ordered else 7) and all(reply["ok"] for reply in replies)
            assert replies[1]["result"].get("predictor", "none") == args.predictor
            assert replies[1]["result"].get("payload_layout", "band") == args.payload_layout
            check["compiled_grid"] = replies[1]["result"]["grid"]
            assert check["compiled_grid"] == replies[0]["result"]["metadata"]["grid"]
            assert replies[3]["result"]["verified"]
            assert not replies[3]["result"]["original_source_opened"]
            assert replies[5]["result"]["bands"]
            if args.ordered:
                ordered = replies[6]["result"]
                assert ordered["numerical_policy"] == "hm_demographics_ordered_v1"
                assert ordered["complete"] and ordered["rows"]
                assert not ordered["provenance"]["summaries_used"]
                check["ordered_bands"] = len(ordered["rows"][0]["bands"])
    receipt = {"passed": True, "expected_crash_control": args.expect_crash,
               "native_stack_bytes": 128 * 1024, "guard_page": True,
               "source_overview": args.overview,
               "ordered_operation": args.ordered,
               "payload_layout": args.payload_layout,
               "library_sha256": hashlib.sha256(args.library.read_bytes()).hexdigest(), "checks": checks}
    (args.scratch / "receipt.json").write_text(json.dumps(receipt, indent=2))
    print(json.dumps(receipt))


if __name__ == "__main__":
    main()
