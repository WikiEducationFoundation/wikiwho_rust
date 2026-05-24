#!/usr/bin/env python3
"""Run the reference wikiwho.py against a captured history.jsonl and
dump the resulting state. The Rust port should produce identical
spam_ids, ordered_revisions, and final token state — any divergence
between this script's output and the Rust binary's `--show-spam-ids`
output is a real algorithm parity bug.

Usage:
  python3 scripts/python_replay.py parity-fixtures/simple/27263/10855732
  python3 scripts/python_replay.py parity-fixtures/en/24544/1354638187 --max-revs 100
  python3 scripts/python_replay.py parity-fixtures/en/24544/1354638187 --whocolor

Outputs JSON-on-stdout with these keys:
  title:           article title from meta.json
  page_id:         page_id from meta.json
  target_rev_id:   the revision the fixture was captured at
  processed:       count of revisions wikiwho processed (non-spam, non-hidden)
  hidden:          count of hidden / textmissing revisions skipped
  spam_count:      len(wikiwho.spam_ids)
  spam_ids:        sorted list of spam rev_ids
  ordered_revisions: list of rev_ids that committed (non-spam, in order)
  paragraphs_ht_size: len(wikiwho.paragraphs_ht)
  sentences_ht_size:  len(wikiwho.sentences_ht)
  token_count:     len(wikiwho.tokens) — total lifetime token allocations

With --whocolor, an extra top-level `whocolor` key is added containing
the wire-shape output of `wikiwho.get_whocolor_data(target_rev_id)`,
post-processed the same way `whocolor/handler.py` does for the public
endpoint (md5-hash anons into class_name, drop editor names). The `age`
field is deliberately omitted because it depends on `datetime.now()` at
script-invocation time; the Rust whocolor-parity comparator excludes it
by default anyway.

The dict shape we feed to wikiwho.analyse_article mirrors the MW Action
API formatversion=1 response (see ../wikiwho_api/api/handler.py:462) —
that's the shape the library was written against. We translate our v2
normalized history.jsonl into v1 shape on the fly.
"""

import argparse
import hashlib
import json
import pathlib
import sys


def load_history(history_path, max_revs=None):
    """Yield revision dicts in the MW Action API formatversion=1 shape
    the reference wikiwho library expects."""
    with history_path.open("r", encoding="utf-8") as fh:
        for i, line in enumerate(fh):
            if max_revs is not None and i >= max_revs:
                break
            entry = json.loads(line)
            if entry["text_hidden"]:
                # Python skips with `if 'texthidden' in revision or
                # 'textmissing' in revision: continue` — mirror that
                # with the v1-style marker.
                yield {
                    "revid": entry["rev_id"],
                    "parentid": entry["parent_id"],
                    "timestamp": entry["timestamp"],
                    "texthidden": "",
                }
                continue
            rev = {
                "revid": entry["rev_id"],
                "parentid": entry["parent_id"],
                "timestamp": entry["timestamp"],
                "*": entry["text"],
            }
            if entry["sha1"] is not None:
                rev["sha1"] = entry["sha1"]
            if entry["comment"] is not None:
                rev["comment"] = entry["comment"]
            if entry["minor"]:
                # v1 marker: presence-only flag with empty-string value.
                rev["minor"] = ""
            if entry["user_id"] is not None:
                rev["userid"] = entry["user_id"]
            if entry["user_name"] is not None:
                rev["user"] = entry["user_name"]
            yield rev


def main():
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("fixture", type=pathlib.Path,
                   help="path to a parity-fixtures/{lang}/{page_id}/{rev_id}/ dir")
    p.add_argument("--max-revs", type=int, default=None,
                   help="only replay the first N revisions (useful for binary-searching divergences)")
    p.add_argument("--lib", type=pathlib.Path, default=pathlib.Path("../wikiwho_api/lib/WikiWho"),
                   help="path to the wikiwho library (default ../wikiwho_api/lib/WikiWho)")
    p.add_argument("--whocolor", action="store_true",
                   help="also emit wire-shape whocolor data (tokens/revisions/biggest_conflict_score)")
    args = p.parse_args()

    if not args.fixture.is_dir():
        sys.exit(f"fixture dir not found: {args.fixture}")
    meta_path = args.fixture / "meta.json"
    history_path = args.fixture / "history.jsonl"
    if not meta_path.exists():
        sys.exit(f"missing meta.json under {args.fixture}")
    if not history_path.exists():
        sys.exit(f"missing history.jsonl under {args.fixture} — run scripts/capture_history.py first")
    meta = json.loads(meta_path.read_text())

    sys.path.insert(0, str(args.lib.resolve()))
    from WikiWho.wikiwho import Wikiwho
    from WikiWho.utils import iter_rev_tokens

    w = Wikiwho(meta["title"])
    revs = list(load_history(history_path, max_revs=args.max_revs))
    w.analyse_article(revs)

    # Build the final-rev token sequence in the same shape as
    # rev_content.json (a list of objects with str/o_rev_id/in/out/etc.).
    # This is what the parity binary diffs against.
    final_tokens = None
    if meta["rev_id"] in w.revisions:
        final = w.revisions[meta["rev_id"]]
        final_tokens = []
        for word in iter_rev_tokens(final):
            final_tokens.append({
                "str": word.value,
                "o_rev_id": word.origin_rev_id,
                "in": list(word.inbound),
                "out": list(word.outbound),
                # token_id is per-article-lifetime; included for diagnostics
                # but the parity binary doesn't compare it (the Rust port's
                # token id assignment is its own arena order, which need not
                # match Python's).
                "token_id": word.token_id,
            })

    summary = {
        "title": meta["title"],
        "page_id": meta["page_id"],
        "target_rev_id": meta["rev_id"],
        "fed_revs": len(revs),
        "processed": len(w.revisions),
        "hidden": sum(1 for r in revs if "texthidden" in r or "textmissing" in r),
        "spam_count": len(w.spam_ids),
        "spam_ids": sorted(w.spam_ids),
        "ordered_revisions": list(w.ordered_revisions),
        "paragraphs_ht_size": len(w.paragraphs_ht),
        "sentences_ht_size": len(w.sentences_ht),
        "paragraphs_total": sum(len(v) for v in w.paragraphs_ht.values()),
        "sentences_total": sum(len(v) for v in w.sentences_ht.values()),
        "token_count": len(w.tokens),
        # Wire-format-shaped final-rev token sequence. None if the target
        # rev_id wasn't reached (e.g. caught as spam, or --max-revs).
        "final_tokens": final_tokens,
    }
    if args.whocolor:
        summary["whocolor"] = build_whocolor(w, meta["rev_id"])
    json.dump(summary, sys.stdout)
    sys.stdout.write("\n")


def _class_name(editor):
    """Mirror `whocolor/handler.py:108-112`: md5 anons, passthrough for
    registered users (and empty for missing)."""
    if isinstance(editor, str) and editor.startswith("0|"):
        return hashlib.md5(editor.encode("utf-8")).hexdigest()
    return editor


def build_whocolor(wikiwho, target_rev_id):
    """Return the wire-shape whocolor data for `target_rev_id` against
    `wikiwho`, or `None` if the rev isn't present (spam or unreached).

    Shape mirrors the post-handler output in `whocolor/handler.py`
    (minus `editor_name` and `extended_html`, plus `age` is omitted):

    - `tokens`: `[[conflict_score, str, o_rev_id, in, out, class_name],
      ...]` in document order.
    - `revisions`: `{rev_id_string: [timestamp, parent_id, class_name]}`.
    - `biggest_conflict_score`: scalar.

    Logic faithfully ported from `wikiwho_simple.get_whocolor_data`
    (`wikiwho_api/wikiwho/wikiwho_simple.py:362-414`), inlined here
    instead of imported because the `wikiwho_simple` module is Django-
    coupled. The `age` field is the only piece we drop — it depends on
    `datetime.now()` at script-invocation time and isn't deterministic.
    Production's md5-hash on anons (`whocolor/handler.py:108-112`) is
    pre-applied so comparing against captured `whocolor.json` works
    field-for-field.
    """
    from WikiWho.utils import iter_rev_tokens

    if target_rev_id not in wikiwho.revisions:
        return None
    revision = wikiwho.revisions[target_rev_id]

    biggest_conflict_score = 0
    wire_tokens = []
    for token in iter_rev_tokens(revision):
        editor_in_prev = None
        conflict_score = 0
        for i, out_rev in enumerate(token.outbound):
            editor_out = wikiwho.revisions[out_rev].editor
            if editor_in_prev is not None and editor_in_prev != editor_out:
                conflict_score += 1
            try:
                in_rev = token.inbound[i]
            except IndexError:
                pass
            else:
                editor_in = wikiwho.revisions[in_rev].editor
                if editor_out != editor_in:
                    conflict_score += 1
                editor_in_prev = editor_in
        if conflict_score > biggest_conflict_score:
            biggest_conflict_score = conflict_score

        editor = wikiwho.revisions[token.origin_rev_id].editor
        wire_tokens.append([
            conflict_score,
            token.value,
            token.origin_rev_id,
            list(token.inbound),
            list(token.outbound),
            _class_name(editor),
        ])

    revisions = {}
    for i, rev_id in enumerate(wikiwho.ordered_revisions):
        parent = 0 if i == 0 else wikiwho.ordered_revisions[i - 1]
        revisions[str(rev_id)] = [
            wikiwho.revisions[rev_id].timestamp,
            parent,
            _class_name(wikiwho.revisions[rev_id].editor),
        ]

    return {
        "tokens": wire_tokens,
        "revisions": revisions,
        "biggest_conflict_score": biggest_conflict_score,
    }


if __name__ == "__main__":
    main()
