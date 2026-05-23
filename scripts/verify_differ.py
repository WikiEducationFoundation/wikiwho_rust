#!/usr/bin/env python3
"""Emit the filtered Differ.compare() output for a set of test cases.

Used as a parity check against `crates/wikiwho-attribute/src/differ.rs`.
The Rust test reads back this JSON and verifies its own differ_compare
matches for each case.

Input:  list of [text_prev, text_curr] pairs on stdin (JSON).
Output: list of [[(tag, value), ...], ...] on stdout (JSON), where tag
        is one of 'keep' | 'delete' | 'insert'.
"""

import json
import sys
from difflib import Differ


def filter_diff(text_prev, text_curr):
    """Run Differ() and convert to (tag, value) tuples, dropping '?' hints."""
    out = []
    for line in Differ().compare(text_prev, text_curr):
        if len(line) < 2:
            continue
        prefix = line[:2]
        value = line[2:]
        if prefix == "  ":
            out.append(["keep", value])
        elif prefix == "- ":
            out.append(["delete", value])
        elif prefix == "+ ":
            out.append(["insert", value])
        # else: '? ' hint — skip
    return out


def main():
    cases = json.load(sys.stdin)
    results = [filter_diff(prev, curr) for prev, curr in cases]
    json.dump(results, sys.stdout)


if __name__ == "__main__":
    main()
