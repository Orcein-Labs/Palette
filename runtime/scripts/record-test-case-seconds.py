#!/usr/bin/env python3
"""Record how long each test case took in CI, for verify_operator.py --shard.

Reads the job logs of a security-suite run (for example
`gh api --allow-escape-sequences repos/OWNER/REPO/actions/jobs/ID/logs > shard1.log`;
without the flag gh writes an empty log) and merges each
case's seconds into runtime/security/test-case-seconds.json. Both hosts share
a case's record. A case's time runs
from the end of the case before it, or from the start of its suite, so the first
case of a suite carries the suite's start-up.
"""
import argparse
import datetime
import json
import re
from pathlib import Path

WEIGHTS = Path(__file__).absolute().parents[1] / "security/test-case-seconds.json"
CASE = re.compile(r"^(test\w+) \(__main__\.(\w+)\.")
ESCAPE = re.compile(r"\x1b\[[0-9;]*m")


def case_seconds(log):
    seconds, suite, case, previous = {}, None, None, None
    for raw in log.read_text(encoding="utf-8-sig", errors="replace").splitlines():
        line = ESCAPE.sub("", raw)
        if len(line) < 28 or line[27] != "Z":
            continue
        at, text = datetime.datetime.fromisoformat(line[:26]), line[28:].strip()
        if text.startswith("verification: "):
            found = re.search(r"/(test_\w+)\.py", text)
            suite, case, previous = found and found.group(1), None, at
            continue
        found = CASE.match(text)
        if found:
            case = f"{found.group(2)}.{found.group(1)}"
        if suite and case and previous and re.search(r"\.\.\. (ok|FAIL|ERROR|skipped)", text):
            seconds[f"{suite}:{case}"] = round((at - previous).total_seconds())
            case, previous = None, at
    return seconds


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("logs", nargs="+", type=Path)
    args = parser.parse_args()
    weights = json.loads(WEIGHTS.read_text()) if WEIGHTS.exists() else {}
    for log in args.logs:
        weights.update(case_seconds(log))
    WEIGHTS.write_text(json.dumps(dict(sorted(weights.items())), indent=1) + "\n")
    print(f"{len(weights)} cases in {WEIGHTS.name}")


if __name__ == "__main__":
    main()
