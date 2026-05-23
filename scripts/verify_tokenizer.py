#!/usr/bin/env python3
"""
Run the reference Python tokenizer (`../wikiwho_api/lib/WikiWho/WikiWho/utils.py`)
on a set of probe strings and print the expected outputs. Used to
verify Rust port parity test assertions during development.

Default probes cover the cases the Rust tokenizer tests check; pass
`--probe TEXT` (repeatable) to add ad-hoc inputs.
"""

import argparse
import json
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO.parent / "wikiwho_api" / "lib" / "WikiWho"))

from WikiWho.utils import (  # noqa: E402
    calculate_hash,
    split_into_paragraphs,
    split_into_sentences,
    split_into_tokens,
)

# Inputs the Rust tests assert against. Keep in sync with the
# `#[test]` cases in crates/wikiwho-attribute/src/tokenize.rs.
PROBES = [
    # hash
    ("hash_md5", ""),
    ("hash_md5", "hello"),
    ("hash_md5", "中国"),
    # paragraphs
    ("split_paragraphs", "a\n\nb"),
    ("split_paragraphs", "a\n\n\nb"),
    ("split_paragraphs", "a\nb"),
    ("split_paragraphs", "a\r\n\r\nb"),
    ("split_paragraphs", "a\r\rb"),
    ("split_paragraphs", "before<table>x</table>after"),
    ("split_paragraphs", "before{|x|}after"),
    # sentences
    ("split_sentences", "foo. bar. baz."),
    ("split_sentences", "hi. ok"),
    ("split_sentences", "a; b? c! d: e"),
    ("split_sentences", "a\nb"),
    ("split_sentences", "a\tb"),
    ("split_sentences", "foo<!--c-->bar"),
    ("split_sentences", "text<ref>cite</ref>more"),
    ("split_sentences", "see http://x.com/a more text"),
    # tokens
    ("split_tokens", "hello world"),
    ("split_tokens", "foo, bar!"),
    ("split_tokens", "[[link]]"),
    ("split_tokens", "{{template}}"),
    ("split_tokens", "a<!--c-->b"),
    ("split_tokens", "a|b"),
    ("split_tokens", "|"),
    ("split_tokens", "||"),
    ("split_tokens", "中国"),
    ("split_tokens", "foo 中国 bar"),
    ("split_tokens", "中。国"),
    ("split_tokens", "just ascii here"),
    ("split_tokens", "price $5"),
    ("split_tokens", "€100"),
    ("split_tokens", ""),
    ("split_tokens", "foo  bar"),
    ("split_tokens", "foo bar"),
]


def run(name, text):
    if name == "hash_md5":
        return calculate_hash(text)
    if name == "split_paragraphs":
        return split_into_paragraphs(text)
    if name == "split_sentences":
        return split_into_sentences(text)
    if name == "split_tokens":
        return list(split_into_tokens(text))
    raise ValueError(name)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--probe", action="append", default=[],
                   metavar="FN:TEXT", help="add ad-hoc probe (FN in "
                   "{hash_md5, split_paragraphs, split_sentences, split_tokens})")
    p.add_argument("--json", action="store_true",
                   help="emit machine-readable JSON instead of human format")
    args = p.parse_args()

    probes = list(PROBES)
    for s in args.probe:
        if ":" not in s:
            raise SystemExit(f"--probe must be FN:TEXT, got {s!r}")
        fn, text = s.split(":", 1)
        probes.append((fn, text))

    if args.json:
        out = [{"fn": fn, "input": text, "output": run(fn, text)}
               for fn, text in probes]
        print(json.dumps(out, ensure_ascii=False, indent=2))
        return

    for fn, text in probes:
        result = run(fn, text)
        print(f"{fn}({text!r}) = {result!r}")


if __name__ == "__main__":
    main()
