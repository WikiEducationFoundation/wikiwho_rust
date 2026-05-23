#!/usr/bin/env python3
"""
Capture parity fixtures from the production wikiwho-api.

For each (lang, title) in the seed list, hits the MW Action API to
resolve title -> (page_id, latest_rev_id), then captures the production
wikiwho-api responses for both endpoints the consumers actually use:

  parity-fixtures/{lang}/{page_id}/{rev_id}/
    rev_content.json   from /{lang}/api/v1.0.0-beta/rev_content/rev_id/{rev_id}/
                       with ?o_rev_id=true&editor=true&token_id=true&in=true&out=true
    whocolor.json      from /{lang}/whocolor/v1.0.0-beta/{title}/{rev_id}/
    meta.json          title, capture timestamp, source URLs, HTTP status codes

These are the byte-for-byte targets the rewrite must reproduce. The rev_id
is frozen at capture time, so the fixture remains stable forever even as
the live article evolves.

Re-running is idempotent: existing (lang, page_id, rev_id) directories are
skipped unless --refresh is passed. Pass --extra LANG:TITLE to add to the
seed list; --only LANG:TITLE to ignore the seed list and capture only
those.

See ../CLAUDE.md "Autonomy posture" for why this script exists and
../PLAN.md section 6 (Phase 1) for the rationale on which articles to
include.
"""

import argparse
import datetime
import json
import pathlib
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

UA = (
    "wikiwho_rust-parity-capture/0.1 "
    "(https://github.com/WikiEducationFoundation; sage@wikiedu.org)"
)
WIKIWHO = "https://wikiwho-api.wmcloud.org"
ROOT = pathlib.Path(__file__).resolve().parent.parent / "parity-fixtures"

# Known-hard articles from PLAN.md section 6 Phase 1 plus a few medium
# and non-English shapes. The first capture should produce ~16 fixtures;
# the corpus grows over time as algorithm work uncovers new edge cases.
ARTICLES = [
    # Known-hard English (the long-tail correctness stress test)
    ("en", "Barack_Obama"),
    ("en", "Donald_Trump"),
    ("en", "COVID-19_pandemic"),
    ("en", "Israel–Hamas_war"),   # en-dash in the title
    ("en", "Adolf_Hitler"),
    ("en", "Jesus"),
    ("en", "Wikipedia"),
    # Medium English (mainstream shapes)
    ("en", "Jesse_Owens"),             # ~6K revs; bench-corpus reference
    ("en", "Paris"),
    ("en", "Albert_Einstein"),
    ("en", "Photosynthesis"),
    # Non-English (token-edge / non-Latin script coverage)
    ("fr", "Paris"),
    ("de", "Berlin"),
    ("simple", "Wikipedia"),
    ("zh", "中国"),            # China
    ("ar", "القاهرة"),  # Cairo (RTL)
]


def _get(url, timeout=300):
    req = urllib.request.Request(url, headers={"User-Agent": UA})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status, resp.read().decode("utf-8")
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", errors="replace")
        return e.code, body


def lookup_page(lang, title):
    """Resolve title -> (page_id, latest_rev_id) via MW Action API."""
    q = urllib.parse.urlencode({
        "action": "query",
        "format": "json",
        "formatversion": 2,
        "prop": "info|revisions",
        "rvprop": "ids",
        "rvlimit": 1,
        "titles": title,
    })
    code, body = _get(f"https://{lang}.wikipedia.org/w/api.php?{q}")
    if code != 200:
        raise RuntimeError(f"MW API returned {code} for {lang}:{title}: {body[:200]}")
    data = json.loads(body)
    pages = data["query"]["pages"]
    if not pages:
        raise RuntimeError(f"no pages returned for {lang}:{title}")
    page = pages[0]
    if page.get("missing"):
        raise RuntimeError(f"page missing: {lang}:{title}")
    revs = page.get("revisions") or []
    if not revs:
        raise RuntimeError(f"no revisions for {lang}:{title} (page_id={page.get('pageid')})")
    return int(page["pageid"]), int(revs[0]["revid"])


def wikiwho_fetch(url, max_attempts=5, base_delay=30):
    """Fetch a wikiwho-api URL with 408/"still processing" retry.

    Returns (status_code, body). Raises RuntimeError if attempts exhausted.

    The production service returns 408 with {"Info": "..."} when an article
    is cold and needs background processing (up to 240-300s per API.md).
    Some endpoints return 200 with {"success": false, "info": "..."} for
    the same condition. Both are retried with exponential backoff.
    """
    for attempt in range(1, max_attempts + 1):
        code, body = _get(url, timeout=600)
        retry_reason = None
        if code == 408:
            retry_reason = "408 still processing"
        elif code == 200:
            try:
                parsed = json.loads(body)
                if isinstance(parsed, dict) and parsed.get("success") is False:
                    info = parsed.get("info") or parsed.get("Info") or ""
                    if "available" in info.lower() or "soon" in info.lower():
                        retry_reason = f"200 success=false: {info[:80]}"
            except (json.JSONDecodeError, AttributeError):
                pass
        if retry_reason is None:
            return code, body
        if attempt == max_attempts:
            raise RuntimeError(f"{retry_reason}; gave up after {max_attempts} attempts")
        delay = base_delay * (2 ** (attempt - 1))
        print(f"    {retry_reason}; sleeping {delay}s before retry {attempt + 1}/{max_attempts}",
              flush=True)
        time.sleep(delay)


def capture_one(lang, title, refresh=False, between=1.5):
    print(f"-> {lang}:{title}", flush=True)
    print(f"   resolving page_id/rev_id via MW API", flush=True)
    page_id, rev_id = lookup_page(lang, title)
    print(f"   page_id={page_id} rev_id={rev_id}", flush=True)

    out = ROOT / lang / str(page_id) / str(rev_id)
    if out.exists() and not refresh:
        print(f"   skip (exists): {out.relative_to(ROOT.parent)}", flush=True)
        return "skipped", out

    out.mkdir(parents=True, exist_ok=True)

    # Fetch each endpoint and write immediately so a later failure doesn't
    # discard earlier success. If one endpoint fails after the other landed,
    # the directory ends up partial — a re-run with --refresh re-fetches both.
    sources = {}

    rc_url = (
        f"{WIKIWHO}/{lang}/api/v1.0.0-beta/rev_content/rev_id/{rev_id}/"
        f"?o_rev_id=true&editor=true&token_id=true&in=true&out=true"
    )
    print(f"   fetching rev_content", flush=True)
    rc_code, rc_body = wikiwho_fetch(rc_url)
    (out / "rev_content.json").write_text(rc_body)
    sources["rev_content"] = {"url": rc_url, "http_status": rc_code,
                              "bytes": len(rc_body)}
    time.sleep(between)

    wc_url = (
        f"{WIKIWHO}/{lang}/whocolor/v1.0.0-beta/"
        f"{urllib.parse.quote(title, safe='')}/{rev_id}/"
    )
    print(f"   fetching whocolor", flush=True)
    try:
        wc_code, wc_body = wikiwho_fetch(wc_url)
        (out / "whocolor.json").write_text(wc_body)
        sources["whocolor"] = {"url": wc_url, "http_status": wc_code,
                               "bytes": len(wc_body)}
        time.sleep(between)
    finally:
        # Always write meta.json, even on partial capture, so the dir is
        # self-describing and re-runs know what's missing.
        (out / "meta.json").write_text(json.dumps({
            "lang": lang,
            "title": title,
            "page_id": page_id,
            "rev_id": rev_id,
            "captured_at": datetime.datetime.now(datetime.timezone.utc)
                              .strftime("%Y-%m-%dT%H:%M:%SZ"),
            "sources": sources,
        }, indent=2, ensure_ascii=False))

    print(f"   wrote {out.relative_to(ROOT.parent)} "
          f"(rev_content={len(rc_body)}B, "
          f"whocolor={sources.get('whocolor', {}).get('bytes', 'MISSING')}B)",
          flush=True)
    return "captured", out


def main():
    p = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    p.add_argument("--refresh", action="store_true",
                   help="re-capture even if fixture directory exists")
    p.add_argument("--extra", action="append", default=[], metavar="LANG:TITLE",
                   help="add article to the seed list (repeatable)")
    p.add_argument("--only", action="append", default=[], metavar="LANG:TITLE",
                   help="capture only these articles, ignoring seed list (repeatable)")
    p.add_argument("--between", type=float, default=1.5,
                   help="seconds to wait between requests (default 1.5; be polite)")
    args = p.parse_args()

    def parse_spec(s):
        if ":" not in s:
            raise SystemExit(f"--extra/--only spec must be LANG:TITLE, got {s!r}")
        lang, title = s.split(":", 1)
        return lang.strip(), title.strip()

    if args.only:
        articles = [parse_spec(s) for s in args.only]
    else:
        articles = list(ARTICLES) + [parse_spec(s) for s in args.extra]

    print(f"capturing {len(articles)} article(s) to {ROOT}", flush=True)
    print(f"polite delay: {args.between}s between requests", flush=True)
    print()

    counts = {"captured": 0, "skipped": 0, "failed": 0}
    failures = []
    for lang, title in articles:
        try:
            status, _ = capture_one(lang, title, refresh=args.refresh,
                                    between=args.between)
            counts[status] += 1
        except Exception as e:
            counts["failed"] += 1
            failures.append((lang, title, repr(e)))
            print(f"   FAILED: {e!r}", flush=True)
        print()

    print(f"done. captured={counts['captured']} "
          f"skipped={counts['skipped']} failed={counts['failed']}")
    if failures:
        print("failures:")
        for lang, title, err in failures:
            print(f"  - {lang}:{title}: {err}")
        sys.exit(1)


if __name__ == "__main__":
    main()
