#!/usr/bin/env python3
"""For a rev_content.json fixture, find tokens whose o_rev_id is in a
target set. Useful for asking 'did production process rev X?'.

Usage:
  python3 scripts/inspect_origins.py rev_content.json 6330300 6330301
"""

import json
import sys
from collections import Counter


def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    path = sys.argv[1]
    targets = {int(x) for x in sys.argv[2:]}
    with open(path, "r", encoding="utf-8") as f:
        rc = json.load(f)
    rev_map = rc["revisions"][0]
    (rev_id, entry), = rev_map.items()
    toks = entry["tokens"]
    matched = []
    by_origin = Counter()
    by_inbound_first = Counter()
    by_outbound_first = Counter()
    for tok in toks:
        o = tok.get("o_rev_id")
        if o in targets:
            matched.append(tok)
        by_origin[o] += 1
        for r in tok.get("in", []):
            if r in targets:
                by_inbound_first[r] += 1
        for r in tok.get("out", []):
            if r in targets:
                by_outbound_first[r] += 1
    print(f"total tokens: {len(toks)}")
    print(f"tokens with o_rev_id in {sorted(targets)}: {len(matched)}")
    for tok in matched[:10]:
        print(f"   {tok['str']!r:25}  o={tok['o_rev_id']} in={tok.get('in',[])[:5]}  out={tok.get('out',[])[:5]}")
    print(f"inbound mentions of targets: {dict(by_inbound_first)}")
    print(f"outbound mentions of targets: {dict(by_outbound_first)}")


if __name__ == "__main__":
    main()
