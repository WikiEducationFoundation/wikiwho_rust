#!/usr/bin/env python3
"""
bench_articleviewer.py — measure ArticleViewer's full request set against
both our test Rust server and the production Python service, capture
storage sizes on both, and (this is the v2 update) properly poll the
"still processing" envelope so the cache-miss timings reflect real
build completion, not first-response latency.

Both servers return an envelope when they haven't built the article yet:

  - whocolor:    HTTP 200, body `{"success": false, "message": "..."}`
  - rev_content: HTTP 408, body `{"success": false, ...}`

ArticleViewer polls with exponential backoff. This script polls at a
fixed interval (default 1s) until the envelope clears or a per-call
hard cap (default 240s) fires.

Run from the laptop. Talks to:

  - en.wikipedia.org              MW Action API (parse, title resolution, users)
  - wikiwho-rs.wmcloud.org        test server (Rust rewrite)
  - wikiwho-api.wmcloud.org       production (Python WikiWho)
  - prod VPS via `ssh wikiwho-api`    one-shot stat of pickle files
    (alias resolves to wikiwho01.wikiwho.eqiad1.wikimedia.cloud)
  - test VPS via `ssh wikiwho-rs`     one-shot du of the per-article dir

The default article set is intentionally small (4 articles, ranging
from a sub-second build to ~85 MB raw test storage) so the full run
fits in well under 10 minutes. Add bigger articles via --titles when
you want them.

Workflow:

  STEP 1 — initial run, with test storage empty
  ---------------------------------------------
  ./scripts/remote-deploy.sh --wipe-storage   # cold test storage
  ./scripts/bench_articleviewer.py --output run1.json

  STEP 2 — measure prod regen
  ---------------------------
  # Delete only the prod pickles whose test cache-miss was < 60s.
  # run1.json prints the page_ids + paths to copy in.
  ssh wikiwho-api 'sudo rm -v /pickles/en/{path1,path2,…}'
  ./scripts/bench_articleviewer.py --output run2.json --prod-only

Per-target measurements, in the order ArticleViewer issues them:
  1. mw_parse              en.wikipedia.org action=parse
  2. whocolor (polling)    until success=true; records total_ms, first_call_ms, n_polls
  3. whocolor (warm)       a second single call, the cache-hit number
  4. rev_content (polling) until status=200; usually returns immediately
                           after whocolor's polling settled
  5. rev_content (warm)    single warm call
  6. mw_users              MW user-info lookup, editors from #4

Output: JSON to --output (default stdout). Summary table to stderr.

Defaults:
  --titles            built-in 4-article sample (override with a file)
  --test-base-url     https://wikiwho-rs.wmcloud.org
  --prod-base-url     https://wikiwho-api.wmcloud.org
  --ssh-host          wikiwho-api  (prod VPS)
  --test-ssh-host     wikiwho-rs   (test VPS)
  --pickle-dir        /pickles/en   (prod; both sharded and legacy-flat handled)
  --test-storage-dir  /var/lib/wikiwho-rs/storage
  --timeout           60      (seconds per single HTTP call)
  --poll-interval     1.0     (seconds between polling attempts)
  --max-poll-s        240     (hard cap per polling call)
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, field, asdict
from typing import Optional

LANG = "en"

# Defaults span: stub (~400KB pickle) → small (~500KB) → medium (~2.6MB).
# Stays well under a 10-minute run with polling. Add bigger articles
# with --titles when needed.
DEFAULT_TITLES = [
    "Delon_Hampton",              # tiny biography, ~400 KB prod pickle
    "Bell-LaPadula_model",        # small technical, ~500 KB
    "Wiki_Education_Foundation",  # small, ~90 KB
    "Photosynthesis",             # medium, ~2.6 MB
]

USER_AGENT = (
    "wikiwho-bench/0.2 (sage@wikiedu.org; "
    "https://github.com/WikiEducationFoundation/wikiwho_rust)"
)


# --------------------------------------------------------------------- HTTP

def _http_get_json(
    url: str, timeout: float
) -> tuple[int, float, int, Optional[dict]]:
    """GET url, return (status, elapsed_ms, body_bytes, parsed_json_or_None)."""
    req = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    t0 = time.perf_counter()
    parsed: Optional[dict] = None
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            body = resp.read()
            elapsed = (time.perf_counter() - t0) * 1000.0
            try:
                parsed = json.loads(body.decode("utf-8"))
            except Exception:
                parsed = None
            return resp.status, elapsed, len(body), parsed
    except urllib.error.HTTPError as e:
        elapsed = (time.perf_counter() - t0) * 1000.0
        try:
            body = e.read() or b""
        except Exception:
            body = b""
        try:
            parsed = json.loads(body.decode("utf-8"))
        except Exception:
            parsed = None
        return e.code, elapsed, len(body), parsed


# --------------------------------------------- still-processing detector

def _is_envelope(status: int, parsed: Optional[dict]) -> bool:
    """Both servers return a still-processing envelope when a build is
    in flight:
      - whocolor:    HTTP 200, JSON `{"success": false, ...}`
      - rev_content: HTTP 408, JSON `{"success": false, ...}`
    """
    if status == 408:
        return True
    if status == 200 and parsed is not None and parsed.get("success") is False:
        return True
    return False


def _wikiwho_call_with_polling(
    url: str,
    *,
    per_call_timeout: float,
    poll_interval: float,
    max_total_s: float,
) -> tuple[dict, Optional[dict]]:
    """Issue GET to url, poll on the still-processing envelope until either
    a ready response or the hard cap fires. Returns ({timing dict}, parsed_json).

    Timing dict keys:
      outcome:        "ready" | "timeout" | "error"
      total_ms:       wall-clock from first request to terminal response
      first_call_ms:  the first call's individual latency
      n_polls:        number of additional calls after the first
      final_status:   HTTP status of the terminal response
      final_bytes:    body bytes of the terminal response
    """
    t0 = time.perf_counter()
    status, ms, body_n, parsed = _http_get_json(url, per_call_timeout)
    first_ms = ms
    n_polls = 0
    while _is_envelope(status, parsed):
        elapsed_s = time.perf_counter() - t0
        if elapsed_s > max_total_s:
            return (
                {
                    "outcome": "timeout",
                    "total_ms": round(elapsed_s * 1000, 1),
                    "first_call_ms": round(first_ms, 1),
                    "n_polls": n_polls,
                    "final_status": status,
                    "final_bytes": body_n,
                },
                parsed,
            )
        time.sleep(poll_interval)
        n_polls += 1
        status, ms, body_n, parsed = _http_get_json(url, per_call_timeout)
    elapsed_s = time.perf_counter() - t0
    outcome = "ready" if status == 200 else "error"
    return (
        {
            "outcome": outcome,
            "total_ms": round(elapsed_s * 1000, 1),
            "first_call_ms": round(first_ms, 1),
            "n_polls": n_polls,
            "final_status": status,
            "final_bytes": body_n,
        },
        parsed,
    )


def _wikiwho_call_warm(url: str, timeout: float) -> dict:
    """A single GET, no polling — for cache-hit measurement."""
    status, ms, body_n, _ = _http_get_json(url, timeout)
    return {"status": status, "ms": round(ms, 1), "bytes": body_n}


# ------------------------------------------------- MW Action API helpers

def resolve_title(title: str, timeout: float) -> dict:
    """Resolve title -> {page_id, latest_rev_id, canonical_title}, following
    redirects. Returns {} on missing pages."""
    params = {
        "action": "query",
        "format": "json",
        "prop": "revisions",
        "rvprop": "ids",
        "rvlimit": "1",
        "titles": title,
        "redirects": "1",
        "formatversion": "2",
    }
    url = f"https://{LANG}.wikipedia.org/w/api.php?" + urllib.parse.urlencode(params)
    _, _, _, data = _http_get_json(url, timeout)
    if not data or "query" not in data:
        return {}
    pages = data["query"].get("pages") or []
    if not pages or pages[0].get("missing"):
        return {}
    p = pages[0]
    revs = p.get("revisions") or [{}]
    return {
        "page_id": p.get("pageid"),
        "canonical_title": p.get("title", title).replace(" ", "_"),
        "latest_rev_id": revs[0].get("revid"),
    }


def time_mw_parse(title: str, rev_id: int, timeout: float) -> dict:
    """ArticleViewer's first request: action=parse with oldid=<rev>.
    See ArticleViewerAPI.js:71 (fetchParsedArticle)."""
    params = {
        "action": "parse",
        "page": title,
        "oldid": str(rev_id),
        "disableeditsection": "true",
        "redirects": "true",
        "format": "json",
        "origin": "*",
    }
    url = f"https://{LANG}.wikipedia.org/w/api.php?" + urllib.parse.urlencode(params)
    status, elapsed_ms, bytes_, _ = _http_get_json(url, timeout)
    return {"status": status, "ms": round(elapsed_ms, 1), "bytes": bytes_}


def time_mw_users(usernames: list[str], timeout: float) -> dict:
    """ArticleViewer's user-info lookup. ususers=u1|u2|… capped at 50.
    See ArticleViewerAPI.js:153."""
    if not usernames:
        return {"status": 0, "ms": 0.0, "bytes": 0, "n_users": 0, "skipped": True}
    capped = usernames[:50]
    params = {
        "action": "query",
        "list": "users",
        "format": "json",
        "ususers": "|".join(capped),
        "origin": "*",
    }
    url = f"https://{LANG}.wikipedia.org/w/api.php?" + urllib.parse.urlencode(params)
    status, elapsed_ms, bytes_, _ = _http_get_json(url, timeout)
    return {
        "status": status,
        "ms": round(elapsed_ms, 1),
        "bytes": bytes_,
        "n_users": len(capped),
    }


# ----------------------------------------------------- wikiwho timing

def _whocolor_url(base_url: str, title: str, rev_id: int) -> str:
    return (
        f"{base_url.rstrip('/')}/{LANG}/whocolor/v1.0.0-beta/"
        f"{urllib.parse.quote(title, safe='')}/{rev_id}/"
    )


def _rev_content_url(base_url: str, title: str) -> str:
    qs = "o_rev_id=true&editor=true&token_id=true&out=true&in=true"
    return (
        f"{base_url.rstrip('/')}/{LANG}/api/v1.0.0-beta/rev_content/"
        f"{urllib.parse.quote(title, safe='')}/?{qs}"
    )


def time_whocolor_polling(
    base_url: str, title: str, rev_id: int, *,
    per_call_timeout: float, poll_interval: float, max_total_s: float,
) -> dict:
    timing, _ = _wikiwho_call_with_polling(
        _whocolor_url(base_url, title, rev_id),
        per_call_timeout=per_call_timeout,
        poll_interval=poll_interval,
        max_total_s=max_total_s,
    )
    return timing


def time_rev_content_polling(
    base_url: str, title: str, *,
    per_call_timeout: float, poll_interval: float, max_total_s: float,
) -> tuple[dict, list[str]]:
    """Time the rev_content call (with polling) and also extract a sample
    of editor usernames from the body for the subsequent MW users query."""
    timing, parsed = _wikiwho_call_with_polling(
        _rev_content_url(base_url, title),
        per_call_timeout=per_call_timeout,
        poll_interval=poll_interval,
        max_total_s=max_total_s,
    )
    editors: list[str] = []
    if parsed:
        # rev_content shape: revisions: [{rev_id: {tokens: [...]}}]
        revs = parsed.get("revisions") or []
        if revs and isinstance(revs[0], dict):
            for rev_blob in revs[0].values():
                if not isinstance(rev_blob, dict):
                    continue
                for tok in rev_blob.get("tokens") or []:
                    ed = tok.get("editor")
                    if (
                        isinstance(ed, str)
                        and not ed.startswith("0|")
                        and ed not in editors
                    ):
                        editors.append(ed)
                        if len(editors) >= 50:
                            break
                if len(editors) >= 50:
                    break
    return timing, editors


def time_whocolor_warm(base_url: str, title: str, rev_id: int, timeout: float) -> dict:
    return _wikiwho_call_warm(_whocolor_url(base_url, title, rev_id), timeout)


def time_rev_content_warm(base_url: str, title: str, timeout: float) -> dict:
    return _wikiwho_call_warm(_rev_content_url(base_url, title), timeout)


# ------------------------------------------------------- SSH helpers

def _run_ssh_sizes(
    ssh_host: str, page_ids: list[int], remote_per_id: str
) -> dict[int, Optional[int]]:
    """Shared helper: SSH once; remote_per_id is the body of a
    `for id in <ids>; do … done` loop that emits exactly one
    '{page_id} {bytes_or_-}' line per id. Returns dict[page_id] = bytes_or_None."""
    if not page_ids:
        return {}
    ids_csv = ",".join(str(i) for i in page_ids)
    remote = f"for id in $(echo {ids_csv} | tr , ' '); do {remote_per_id} done"
    proc = subprocess.run(
        ["ssh", ssh_host, remote],
        capture_output=True,
        text=True,
        timeout=180,
    )
    if proc.returncode != 0:
        print(
            f"[ssh] {ssh_host}: WARN rc={proc.returncode}; "
            f"stderr: {proc.stderr.strip()[:400]}",
            file=sys.stderr,
        )
    out: dict[int, Optional[int]] = {pid: None for pid in page_ids}
    for line in (proc.stdout or "").splitlines():
        parts = line.split()
        if len(parts) != 2:
            continue
        try:
            pid = int(parts[0])
        except ValueError:
            continue
        out[pid] = None if parts[1] == "-" else int(parts[1])
    return out


def ssh_pickle_sizes(
    ssh_host: str, pickle_dir: str, page_ids: list[int]
) -> dict[int, Optional[int]]:
    """Single SSH call. Looks for the prod pickle in either layout:
      - Sharded:  {pickle_dir}/<(id//1000)*1000>/<id>.p   (the common case)
      - Flat:     {pickle_dir}/<id>.p                     (legacy)
    Prints bytes of whichever exists, or '-' if neither does."""
    print(
        f"[ssh] {ssh_host}: stat pickle sizes for {len(page_ids)} ids",
        file=sys.stderr,
    )
    body = (
        'bucket=$(( (id / 1000) * 1000 )); '
        f's="{pickle_dir}/$bucket/$id.p"; '
        f'f="{pickle_dir}/$id.p"; '
        'if [ -f "$s" ]; then printf "%s %s\\n" "$id" "$(stat -c %s "$s")"; '
        'elif [ -f "$f" ]; then printf "%s %s\\n" "$id" "$(stat -c %s "$f")"; '
        'else printf "%s -\\n" "$id"; fi;'
    )
    return _run_ssh_sizes(ssh_host, page_ids, body)


def ssh_test_storage_sizes(
    ssh_host: str, storage_dir: str, language: str, page_ids: list[int]
) -> dict[int, Optional[int]]:
    """Single SSH call; for each id, sudo-du the sharded article dir under
    <storage_dir>/<lang>/<id//1M>/<id//1k>/<id>/. Sudo is required because
    the parent dir is mode 750."""
    print(
        f"[ssh] {ssh_host}: sudo du test storage for {len(page_ids)} ids",
        file=sys.stderr,
    )
    body = (
        'major=$((id / 1000000)); minor=$((id / 1000)); '
        f'd="{storage_dir}/{language}/$major/$minor/$id"; '
        'if sudo test -d "$d"; then '
        '  sz=$(sudo du -bs "$d" 2>/dev/null | cut -f1); '
        '  printf "%s %s\\n" "$id" "${sz:--}"; '
        'else printf "%s -\\n" "$id"; fi;'
    )
    return _run_ssh_sizes(ssh_host, page_ids, body)


# -------------------------------------------------------------- driver

@dataclass
class TargetResult:
    """Timings against one wikiwho server, in ArticleViewer's order.
    `*_first` are polling-aware (built-in retry on still-processing
    envelope); `*_warm` are a single cache-hit call."""
    mw_parse: dict = field(default_factory=dict)
    whocolor_first: dict = field(default_factory=dict)
    whocolor_warm: dict = field(default_factory=dict)
    rev_content_first: dict = field(default_factory=dict)
    rev_content_warm: dict = field(default_factory=dict)
    mw_users: dict = field(default_factory=dict)
    n_editors_found: int = 0


@dataclass
class ArticleResult:
    title: str
    canonical_title: Optional[str] = None
    page_id: Optional[int] = None
    latest_rev_id: Optional[int] = None
    prod_pickle_bytes: Optional[int] = None
    test_storage_bytes: Optional[int] = None
    error: Optional[str] = None
    test: Optional[TargetResult] = None
    prod: Optional[TargetResult] = None


def _fmt_polling(d: dict) -> str:
    if not d:
        return "-"
    return (
        f"total={d.get('total_ms', '-'):>7}ms "
        f"first={d.get('first_call_ms', '-'):>6}ms "
        f"polls={d.get('n_polls', '-'):>3} "
        f"status={d.get('final_status', '-')} "
        f"bytes={d.get('final_bytes', 0):>9} "
        f"outcome={d.get('outcome', '-')}"
    )


def run_target(
    label: str,
    base_url: str,
    canonical_title: str,
    rev_id: int,
    args: argparse.Namespace,
) -> TargetResult:
    t = TargetResult()

    print(f"  [{label}] mw parse …", file=sys.stderr)
    t.mw_parse = time_mw_parse(canonical_title, rev_id, args.timeout)
    print(f"  [{label}]   {t.mw_parse}", file=sys.stderr)

    print(f"  [{label}] whocolor (polling) …", file=sys.stderr)
    t.whocolor_first = time_whocolor_polling(
        base_url, canonical_title, rev_id,
        per_call_timeout=args.timeout,
        poll_interval=args.poll_interval,
        max_total_s=args.max_poll_s,
    )
    print(f"  [{label}]   {_fmt_polling(t.whocolor_first)}", file=sys.stderr)

    print(f"  [{label}] whocolor (warm) …", file=sys.stderr)
    t.whocolor_warm = time_whocolor_warm(
        base_url, canonical_title, rev_id, args.timeout
    )
    print(f"  [{label}]   {t.whocolor_warm}", file=sys.stderr)

    print(f"  [{label}] rev_content (polling) …", file=sys.stderr)
    t.rev_content_first, editors = time_rev_content_polling(
        base_url, canonical_title,
        per_call_timeout=args.timeout,
        poll_interval=args.poll_interval,
        max_total_s=args.max_poll_s,
    )
    print(
        f"  [{label}]   {_fmt_polling(t.rev_content_first)} editors={len(editors)}",
        file=sys.stderr,
    )

    print(f"  [{label}] rev_content (warm) …", file=sys.stderr)
    t.rev_content_warm = time_rev_content_warm(
        base_url, canonical_title, args.timeout
    )
    print(f"  [{label}]   {t.rev_content_warm}", file=sys.stderr)

    t.n_editors_found = len(editors)
    t.mw_users = time_mw_users(editors, args.timeout)
    print(f"  [{label}]   mw_users {t.mw_users}", file=sys.stderr)

    return t


def process_article(title: str, args: argparse.Namespace) -> ArticleResult:
    print(f"\n=== {title} ===", file=sys.stderr)
    out = ArticleResult(title=title)

    print("  resolve title -> page_id/latest_rev …", file=sys.stderr)
    meta = resolve_title(title, timeout=args.timeout)
    if not meta:
        out.error = "title resolution failed (missing or hidden)"
        print(f"  ERROR: {out.error}", file=sys.stderr)
        return out
    out.canonical_title = meta["canonical_title"]
    out.page_id = meta["page_id"]
    out.latest_rev_id = meta["latest_rev_id"]
    print(
        f"  -> page_id={out.page_id} rev_id={out.latest_rev_id} "
        f"canonical={out.canonical_title}",
        file=sys.stderr,
    )

    if not args.prod_only:
        out.test = run_target(
            "test", args.test_base_url, out.canonical_title, out.latest_rev_id, args
        )
    if not args.test_only:
        out.prod = run_target(
            "prod", args.prod_base_url, out.canonical_title, out.latest_rev_id, args
        )
    return out


def summarize(results: list[ArticleResult]) -> str:
    rows = []
    header = (
        "title".ljust(34)
        + "page_id".rjust(10)
        + "prod_kb".rjust(10)
        + "test_kb".rjust(10)
        + "test_miss_s".rjust(13)
        + "test_polls".rjust(11)
        + "test_warm_ms".rjust(13)
        + "prod_miss_s".rjust(13)
        + "prod_warm_ms".rjust(13)
    )
    rows.append(header)
    rows.append("-" * len(header))

    def kb(b: Optional[int]) -> str:
        if b is None:
            return "missing"
        if b == -1:
            return "n/a"
        return f"{b // 1024}"

    def polling_total_s(d: dict) -> str:
        if not d:
            return "-"
        ms = d.get("total_ms")
        if ms is None:
            return "-"
        return f"{ms / 1000:.1f}"

    def polling_polls(d: dict) -> str:
        if not d:
            return "-"
        n = d.get("n_polls")
        return "-" if n is None else f"{n}"

    def warm_ms(d: dict) -> str:
        if not d:
            return "-"
        ms = d.get("ms")
        return "-" if ms is None else f"{ms:.0f}"

    for r in results:
        if r.error:
            rows.append(f"{r.title[:33]:<34}ERROR: {r.error[:60]}")
            continue
        rows.append(
            r.title[:33].ljust(34)
            + str(r.page_id or "?").rjust(10)
            + kb(r.prod_pickle_bytes).rjust(10)
            + kb(r.test_storage_bytes).rjust(10)
            + polling_total_s(getattr(r.test, "whocolor_first", {}) if r.test else {}).rjust(13)
            + polling_polls(getattr(r.test, "whocolor_first", {}) if r.test else {}).rjust(11)
            + warm_ms(getattr(r.test, "whocolor_warm", {}) if r.test else {}).rjust(13)
            + polling_total_s(getattr(r.prod, "whocolor_first", {}) if r.prod else {}).rjust(13)
            + warm_ms(getattr(r.prod, "whocolor_warm", {}) if r.prod else {}).rjust(13)
        )
    return "\n".join(rows)


def main() -> int:
    p = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    p.add_argument("--titles", default=None,
                   help="file of one en.wp title per line (default: built-in 4-article sample)")
    p.add_argument("--output", default="-", help="JSON output path (default: stdout)")
    p.add_argument("--test-base-url", default="https://wikiwho-rs.wmcloud.org")
    p.add_argument("--prod-base-url", default="https://wikiwho-api.wmcloud.org")
    p.add_argument("--ssh-host", default="wikiwho-api",
                   help="prod VPS SSH alias for pickle stat")
    p.add_argument("--pickle-dir", default="/pickles/en",
                   help="prod pickle root; both sharded <bucket>/<id>.p and flat <id>.p tried")
    p.add_argument("--test-ssh-host", default="wikiwho-rs",
                   help="test VPS SSH alias for storage du")
    p.add_argument("--test-storage-dir", default="/var/lib/wikiwho-rs/storage",
                   help="test storage root containing <lang>/<id//1M>/<id//1k>/<id>/")
    p.add_argument("--timeout", type=float, default=60.0,
                   help="per-HTTP-request timeout in seconds")
    p.add_argument("--poll-interval", type=float, default=1.0,
                   help="seconds between polls while still-processing envelope persists")
    p.add_argument("--max-poll-s", type=float, default=240.0,
                   help="hard cap per polling call (whocolor or rev_content)")
    p.add_argument("--skip-storage", action="store_true",
                   help="don't SSH for storage sizes")
    p.add_argument("--test-only", action="store_true")
    p.add_argument("--prod-only", action="store_true",
                   help="use after deleting prod pickles to measure regen")
    args = p.parse_args()

    if args.test_only and args.prod_only:
        print("error: --test-only and --prod-only are mutually exclusive", file=sys.stderr)
        return 2

    if args.titles:
        with open(args.titles) as f:
            titles = [
                ln.strip().replace(" ", "_")
                for ln in f
                if ln.strip() and not ln.startswith("#")
            ]
    else:
        titles = list(DEFAULT_TITLES)

    print(f"=== bench_articleviewer v0.2 ({len(titles)} articles) ===", file=sys.stderr)
    print(f"  test_base_url   = {args.test_base_url}", file=sys.stderr)
    print(f"  prod_base_url   = {args.prod_base_url}", file=sys.stderr)
    if not args.skip_storage:
        print(f"  prod stat       = {args.ssh_host}:{args.pickle_dir}", file=sys.stderr)
        print(f"  test stat       = {args.test_ssh_host}:{args.test_storage_dir}", file=sys.stderr)
    print(
        f"  poll            = every {args.poll_interval}s, cap {args.max_poll_s}s/call",
        file=sys.stderr,
    )
    print(
        f"  scope           = "
        f"{'prod-only' if args.prod_only else 'test-only' if args.test_only else 'test+prod'}",
        file=sys.stderr,
    )

    run_start = time.perf_counter()
    results: list[ArticleResult] = []
    for title in titles:
        try:
            r = process_article(title, args)
        except Exception as e:
            r = ArticleResult(title=title, error=f"uncaught: {e!r}")
        results.append(r)

    page_ids = [r.page_id for r in results if r.page_id]
    if not args.skip_storage:
        sizes = ssh_pickle_sizes(args.ssh_host, args.pickle_dir, page_ids)
        for r in results:
            if r.page_id is not None:
                r.prod_pickle_bytes = sizes.get(r.page_id)
        if not args.prod_only:
            test_sizes = ssh_test_storage_sizes(
                args.test_ssh_host, args.test_storage_dir, LANG, page_ids
            )
            for r in results:
                if r.page_id is not None:
                    r.test_storage_bytes = test_sizes.get(r.page_id)
        else:
            for r in results:
                r.test_storage_bytes = -1
    else:
        for r in results:
            r.prod_pickle_bytes = -1
            r.test_storage_bytes = -1

    elapsed_total = time.perf_counter() - run_start
    payload = {
        "test_base_url": args.test_base_url,
        "prod_base_url": args.prod_base_url,
        "ssh_host": args.ssh_host if not args.skip_storage else None,
        "test_ssh_host": args.test_ssh_host if not args.skip_storage else None,
        "scope": (
            "prod-only" if args.prod_only else "test-only" if args.test_only else "test+prod"
        ),
        "poll_interval_s": args.poll_interval,
        "max_poll_s": args.max_poll_s,
        "elapsed_total_s": round(elapsed_total, 1),
        "results": [asdict(r) for r in results],
    }

    out_json = json.dumps(payload, indent=2)
    if args.output == "-":
        print(out_json)
    else:
        with open(args.output, "w") as f:
            f.write(out_json)
        print(f"\nwrote {args.output}", file=sys.stderr)

    print(f"\ntotal elapsed: {elapsed_total:.1f}s", file=sys.stderr)
    print("\n" + summarize(results), file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
