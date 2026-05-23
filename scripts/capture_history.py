#!/usr/bin/env python3
"""
Capture per-fixture revision history from the MW Action API.

For each parity-fixtures/{lang}/{page_id}/{rev_id}/meta.json, fetch the
full revision history of {page_id} up to and including {rev_id} from
{lang}.wikipedia.org, oldest-first, and write it as
`{fixture}/history.jsonl` — one JSON object per line.

Each line:
  {
    "rev_id":      int,
    "parent_id":   int,            # 0 for the first revision
    "timestamp":   str,            # e.g. "2024-01-01T00:00:00Z"
    "sha1":        str | null,     # MW-supplied content hash; null on hidden
    "comment":     str | null,     # null on commenthidden / suppressed
    "minor":       bool,
    "user_id":     int | null,     # null on userhidden / suppressed
    "user_name":   str | null,     # null on userhidden / suppressed
    "text":        str,            # "" when text is hidden/missing
    "text_hidden": bool            # true if texthidden / textmissing
  }

This mirrors the input shape `Wikiwho.analyse_article` (Python reference,
`wikiwho.py:144`) consumes — `text_hidden=True` revisions are the ones
the reference skips with `continue`. The Rust caller mirrors that skip.

Idempotent: skip a fixture whose history.jsonl already ends with the
target rev_id. Use `--refresh` to force re-fetch.

Common usage:
  # Capture history for every fixture with < 500 revs (saves you from
  # accidentally pulling Obama's 57K-rev history overnight):
  python3 scripts/capture_history.py --max-revs 500

  # Force a single fixture:
  python3 scripts/capture_history.py --only en/79023819 --refresh

The script is polite: 0.5s between batches by default; `--between SECS`
to override. Use `--bot-token TOKEN` (paired with an account that has
the `apihighlimits` right) to bump rvlimit to 500.

Caveats:
  - The MW Action API pages oldest-first with rvlimit≤50 for anon users,
    so an Obama-class article needs ~1100 batches even with rvlimit=max.
    Expect long runtimes (and tens of GB of disk) on the biggest
    articles. The `--max-revs` cap exists so you can validate the
    pipeline on small fixtures first.
  - rev_id 0 (the placeholder used in the wikiwho algorithm before any
    revision lands) never appears here; the earliest real rev gets the
    article going.
  - We use page_id (stable across page moves) rather than title.
"""

import argparse
import json
import os
import pathlib
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

UA = (
    "wikiwho_rust-history-capture/0.1 "
    "(https://github.com/WikiEducationFoundation; sage@wikiedu.org)"
)
ROOT = pathlib.Path(__file__).resolve().parent.parent / "parity-fixtures"

# Fields we ask the MW API for. Matches the production wikiwho-api
# request at ../wikiwho_api/api/handler.py:462.
RVPROP = "ids|timestamp|user|userid|comment|flags|sha1|content"


def _request_json(url, headers=None, max_attempts=5, base_delay=2):
    """Fetch a URL, retrying on transient errors (503/429/network).

    Returns the parsed JSON body. Raises RuntimeError if attempts
    exhausted.
    """
    last_err = None
    for attempt in range(1, max_attempts + 1):
        try:
            req = urllib.request.Request(url, headers=headers or {})
            req.add_header("User-Agent", UA)
            with urllib.request.urlopen(req, timeout=180) as resp:
                if resp.status >= 500:
                    raise RuntimeError(f"HTTP {resp.status}")
                body = resp.read().decode("utf-8")
                return json.loads(body)
        except urllib.error.HTTPError as e:
            # 429 (rate-limit) and 5xx are retriable; 4xx is not.
            if e.code in (429, 503, 504):
                last_err = f"HTTP {e.code}: {e.reason}"
                retry_after = e.headers.get("Retry-After")
                delay = (
                    int(retry_after) if retry_after and retry_after.isdigit()
                    else base_delay * (2 ** (attempt - 1))
                )
            else:
                # Non-retriable: surface the body for diagnostics.
                try:
                    body = e.read().decode("utf-8", errors="replace")
                except Exception:
                    body = "<unreadable>"
                raise RuntimeError(f"HTTP {e.code} {e.reason}: {body[:300]}") from e
        except (urllib.error.URLError, json.JSONDecodeError, RuntimeError) as e:
            last_err = repr(e)
            delay = base_delay * (2 ** (attempt - 1))

        if attempt == max_attempts:
            raise RuntimeError(f"exhausted retries: {last_err}")
        print(
            f"    transient error ({last_err}); sleeping {delay}s "
            f"before retry {attempt + 1}/{max_attempts}",
            flush=True,
        )
        time.sleep(delay)


def normalize_revision(rev):
    """Convert one MW Action API revision object to the JSONL shape.

    The MW Action API revision shape is documented at
    https://www.mediawiki.org/wiki/API:Revisions; we only consume the
    subset described in this script's module docstring. The output is
    a plain dict ready for json.dumps.

    formatversion quirks (we use formatversion=2):
      - `minor` is always present as a bool (True/False). formatversion=1
        omitted the key when the edit wasn't minor, so the natural
        "minor" in rev check would only work there. Use the value.
      - `userhidden`, `commenthidden`, `suppressed`, `sha1hidden` still
        use presence-when-true semantics in v2 (omitted when the flag
        isn't set). The "X" in rev check is fine for these.
      - `texthidden` lives under `slots.main.texthidden = True` in v2,
        never at the top level; we check the slot below.
    """
    slot = (rev.get("slots") or {}).get("main") or {}
    text_hidden = (
        slot.get("texthidden") is True
        or "textmissing" in rev
        or "suppressed" in rev
    )
    text = ""
    if not text_hidden:
        # rvslots=main puts content under slots.main.content in v2 and
        # slots.main.* in legacy responses; tolerate both.
        text = slot.get("content")
        if text is None:
            text = slot.get("*") or rev.get("*", "")

    user_hidden = "userhidden" in rev
    comment_hidden = "commenthidden" in rev or "suppressed" in rev

    return {
        "rev_id":      int(rev["revid"]),
        "parent_id":   int(rev.get("parentid", 0)),
        "timestamp":   rev["timestamp"],
        "sha1":        rev.get("sha1"),
        "comment":     None if comment_hidden else rev.get("comment"),
        "minor":       bool(rev.get("minor", False)),
        "user_id":     None if user_hidden else rev.get("userid"),
        "user_name":   None if user_hidden else rev.get("user"),
        "text":        text,
        "text_hidden": text_hidden,
    }


def already_complete(history_path, target_rev_id):
    """Return True if the last line of history.jsonl matches target_rev_id."""
    if not history_path.exists():
        return False
    # Tail-read the last line without slurping the file (these can be huge).
    with history_path.open("rb") as f:
        try:
            f.seek(-1, os.SEEK_END)
        except OSError:
            return False  # empty file
        if f.tell() == 0:
            return False
        # Walk backward to find the last newline.
        f.seek(0, os.SEEK_END)
        size = f.tell()
        chunk = 4096
        pos = max(0, size - chunk)
        f.seek(pos)
        tail = f.read()
        # Find the last newline before the very end.
        try:
            last_line = tail.rstrip(b"\n").split(b"\n")[-1]
        except IndexError:
            return False
    try:
        obj = json.loads(last_line)
    except json.JSONDecodeError:
        return False
    return obj.get("rev_id") == target_rev_id


def capture_history(meta_path, max_revs=None, between=0.5, refresh=False):
    """Capture history.jsonl for the fixture at `meta_path`.

    Returns the count of revisions written, or 0 if skipped/short-
    circuited.
    """
    meta = json.loads(meta_path.read_text())
    lang = meta["lang"]
    page_id = meta["page_id"]
    target_rev_id = meta["rev_id"]
    title = meta["title"]

    out = meta_path.parent / "history.jsonl"
    rel = out.relative_to(ROOT.parent)

    if not refresh and already_complete(out, target_rev_id):
        print(f"   skip (history.jsonl already ends at rev_id={target_rev_id}): {rel}",
              flush=True)
        return 0

    api = f"https://{lang}.wikipedia.org/w/api.php"
    params = {
        "action":      "query",
        "format":      "json",
        "formatversion": "2",
        "prop":        "revisions",
        "rvprop":      RVPROP,
        "rvlimit":     "max",
        "rvdir":       "newer",
        "rvslots":     "main",
        "rvendid":     str(target_rev_id),
        "pageids":     str(page_id),
    }

    print(f"-> {lang}:{title} (page_id={page_id}, up to rev_id={target_rev_id})",
          flush=True)

    batches = 0
    written = 0
    saw_target = False
    # Write directly to disk so a crash 1000 batches in doesn't lose
    # everything. Truncate when re-fetching.
    with out.open("w", encoding="utf-8") as fh:
        rvcontinue = None
        while True:
            batches += 1
            q = dict(params)
            if rvcontinue is not None:
                q["rvcontinue"] = rvcontinue
            url = f"{api}?{urllib.parse.urlencode(q)}"
            data = _request_json(url)

            err = data.get("error")
            if err:
                raise RuntimeError(f"MW API error: {err}")

            pages = (data.get("query") or {}).get("pages") or []
            if not pages:
                raise RuntimeError(f"no pages for page_id={page_id}: {data!r:200}")
            page = pages[0]
            if page.get("missing"):
                raise RuntimeError(f"page missing: page_id={page_id}")

            revs = page.get("revisions") or []
            for rev in revs:
                norm = normalize_revision(rev)
                fh.write(json.dumps(norm, ensure_ascii=False))
                fh.write("\n")
                written += 1
                if norm["rev_id"] == target_rev_id:
                    saw_target = True

            if batches == 1 or batches % 10 == 0 or saw_target:
                print(f"   batch {batches}: wrote {len(revs)} revs "
                      f"(total {written}, last rev_id={revs[-1]['revid'] if revs else 'none'})",
                      flush=True)

            if max_revs is not None and written >= max_revs and not saw_target:
                # We were asked to stop early — and we didn't hit the
                # target. Write a sentinel into the file so the parity
                # binary knows this is a partial history. Actually we
                # report instead, then delete the file (a partial
                # history is worse than no history).
                fh.flush()
                print(f"   ABORT: hit --max-revs={max_revs} before reaching "
                      f"target rev_id={target_rev_id}. Article has more "
                      f"revisions than the cap allows. Skipping fixture "
                      f"(removing partial history.jsonl).", flush=True)
                fh.close()
                out.unlink()
                return -1

            if saw_target:
                break

            cont = data.get("continue")
            if not cont or "rvcontinue" not in cont:
                # API says there are no more revisions, but we never
                # saw the target. That's an error — either the target
                # rev_id doesn't belong to this page, or it's been
                # deleted.
                if not saw_target:
                    raise RuntimeError(
                        f"finished pagination without seeing rev_id={target_rev_id}"
                    )
                break
            rvcontinue = cont["rvcontinue"]
            time.sleep(between)

    print(f"   wrote {written} revisions to {rel} in {batches} batches",
          flush=True)
    return written


def main():
    p = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    p.add_argument("--refresh", action="store_true",
                   help="re-capture even if history.jsonl is already complete")
    p.add_argument("--only", action="append", default=[], metavar="LANG/PAGE_ID",
                   help="only run for the given fixture(s); accepts substrings")
    p.add_argument("--max-revs", type=int, default=None, metavar="N",
                   help="abort a fixture if its history exceeds N revisions; "
                        "useful for validating the pipeline before pulling "
                        "Obama-class articles")
    p.add_argument("--between", type=float, default=0.5,
                   help="seconds to wait between batches (default 0.5)")
    args = p.parse_args()

    fixtures = sorted(ROOT.rglob("meta.json"))
    if not fixtures:
        sys.exit(f"no fixtures under {ROOT} — run scripts/capture_fixtures.py first")

    if args.only:
        def matches(path):
            meta = json.loads(path.read_text())
            key = f"{meta['lang']}/{meta['page_id']}"
            return any(f in key or f == key for f in args.only)
        fixtures = [f for f in fixtures if matches(f)]
        if not fixtures:
            sys.exit(f"no fixtures matched {args.only}")

    print(f"capturing history for {len(fixtures)} fixture(s)", flush=True)
    print(f"polite delay: {args.between}s between batches", flush=True)
    if args.max_revs:
        print(f"abort threshold: {args.max_revs} revisions", flush=True)
    print()

    counts = {"captured": 0, "skipped": 0, "aborted": 0, "failed": 0}
    failures = []
    for meta_path in fixtures:
        try:
            n = capture_history(
                meta_path,
                max_revs=args.max_revs,
                between=args.between,
                refresh=args.refresh,
            )
            if n == 0:
                counts["skipped"] += 1
            elif n == -1:
                counts["aborted"] += 1
            else:
                counts["captured"] += 1
        except Exception as e:
            counts["failed"] += 1
            failures.append((str(meta_path.relative_to(ROOT.parent)), repr(e)))
            print(f"   FAILED: {e!r}", flush=True)
        print()

    print(f"done. captured={counts['captured']} skipped={counts['skipped']} "
          f"aborted={counts['aborted']} failed={counts['failed']}")
    if failures:
        print("failures:")
        for path, err in failures:
            print(f"  - {path}: {err}")
        sys.exit(1)


if __name__ == "__main__":
    main()
