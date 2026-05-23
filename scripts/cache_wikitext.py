#!/usr/bin/env python3
"""
Cache wikitext per parity fixture.

For each `parity-fixtures/{lang}/{page_id}/{rev_id}/meta.json`, fetch the
wikitext of `rev_id` from the MW Action API and write it to
`{fixture}/wikitext.txt`. Idempotent — existing wikitext is left alone
unless `--refresh` is passed.

The Rust parity-check binary reads these files to drive its tokenizer
and compare output to the captured `rev_content.json`. The captured
fixtures themselves are post-algorithm outputs and don't include the
source wikitext, so this step lands the input side of the parity test.
"""

import argparse
import json
import pathlib
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent / "parity-fixtures"
UA = (
    "wikiwho_rust-parity-capture/0.1 "
    "(https://github.com/WikiEducationFoundation; sage@wikiedu.org)"
)


def fetch_wikitext(lang, rev_id):
    """Return the wikitext of `rev_id` on `lang.wikipedia.org`.

    Uses MW Action API with formatversion=2 and rvslots=main, mirroring
    the modern convention.
    """
    q = urllib.parse.urlencode({
        "action": "query",
        "format": "json",
        "formatversion": 2,
        "prop": "revisions",
        "rvprop": "content",
        "rvslots": "main",
        "revids": rev_id,
    })
    url = f"https://{lang}.wikipedia.org/w/api.php?{q}"
    req = urllib.request.Request(url, headers={"User-Agent": UA})
    with urllib.request.urlopen(req, timeout=180) as resp:
        data = json.load(resp)
    pages = data.get("query", {}).get("pages") or []
    if not pages:
        raise RuntimeError(f"no page in response for {lang}:{rev_id}: {data!r:200}")
    page = pages[0]
    revs = page.get("revisions") or []
    if not revs:
        raise RuntimeError(
            f"no revision in page for {lang}:{rev_id} "
            f"(page_id={page.get('pageid')}): {page!r:200}"
        )
    slots = revs[0].get("slots") or {}
    main = slots.get("main") or {}
    content = main.get("content")
    if content is None:
        raise RuntimeError(f"no main slot content for {lang}:{rev_id}: {revs[0]!r:200}")
    return content


def main():
    p = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    p.add_argument("--refresh", action="store_true",
                   help="re-fetch even if wikitext.txt exists")
    p.add_argument("--between", type=float, default=1.0,
                   help="seconds between requests (default 1.0; be polite)")
    args = p.parse_args()

    fixtures = sorted(ROOT.rglob("meta.json"))
    if not fixtures:
        sys.exit(f"no fixtures under {ROOT} — run scripts/capture_fixtures.py first")
    print(f"caching wikitext for {len(fixtures)} fixture(s)", flush=True)
    print()

    counts = {"cached": 0, "skipped": 0, "failed": 0}
    failures = []
    for meta_path in fixtures:
        meta = json.loads(meta_path.read_text())
        lang = meta["lang"]
        rev_id = meta["rev_id"]
        title = meta["title"]
        target = meta_path.parent / "wikitext.txt"
        rel = target.relative_to(ROOT.parent)

        if target.exists() and not args.refresh:
            counts["skipped"] += 1
            continue

        try:
            print(f"-> {lang}:{title} rev_id={rev_id}", flush=True)
            text = fetch_wikitext(lang, rev_id)
            target.write_text(text)
            counts["cached"] += 1
            print(f"   wrote {len(text)} chars to {rel}", flush=True)
            time.sleep(args.between)
        except Exception as e:
            counts["failed"] += 1
            failures.append((lang, rev_id, repr(e)))
            print(f"   FAILED: {e!r}", flush=True)

    print()
    print(f"done. cached={counts['cached']} skipped={counts['skipped']} "
          f"failed={counts['failed']}")
    if failures:
        print("failures:")
        for lang, rev_id, err in failures:
            print(f"  - {lang}:{rev_id}: {err}")
        sys.exit(1)


if __name__ == "__main__":
    main()
