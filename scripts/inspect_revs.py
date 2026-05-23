#!/usr/bin/env python3
"""Inspect specific revisions in a history.jsonl by rev_id.

Usage:
  python3 scripts/inspect_revs.py history.jsonl 6330300 6330301
"""

import json
import sys


def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    path = sys.argv[1]
    wanted = {int(x) for x in sys.argv[2:]}
    with open(path, "r", encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            r = json.loads(line)
            if r["rev_id"] in wanted:
                tl = len(r["text"])
                preview = r["text"][:160].replace("\n", " | ")
                comment = r["comment"]
                print(f"rev {r['rev_id']}  parent {r['parent_id']}  minor={r['minor']}  "
                      f"user={r['user_name']!r}")
                print(f"   comment={comment!r}")
                print(f"   text_len={tl} sha1={r['sha1']}")
                print(f"   preview={preview!r}")
                print()


if __name__ == "__main__":
    main()
