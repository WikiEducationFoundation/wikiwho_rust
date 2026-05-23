# WikiWho attribution algorithm — specification for parity

This document captures the semantics of the reference implementation in
`../wikiwho_api/lib/WikiWho/WikiWho/wikiwho.py` precisely enough that an
independent reimplementation can be checked for parity. Every claim here
should be verifiable against the source; line numbers refer to that file
unless otherwise noted.

The goal of the algorithm is, given a sequence of revisions of a wiki
article, to assign to each *token* (word) an origin revision (which
revision first introduced it) and a full history of *outbound*
(deletion) and *inbound* (reintroduction) revision ids. Two tokens with
the same string value but in different positions get distinct ids, and
tokens that survive a delete/reintroduce round-trip retain their
original id.

## 1. Inputs

Each revision provides:

- `id` — Wikipedia revision id (`int`)
- `timestamp` — ISO 8601 string
- `text` — wikitext content (string; may be `''` for hidden/missing)
- `sha1` — content SHA-1 hex string (optional; if absent we compute one)
- `comment` — edit summary (optional)
- `minor` — boolean (optional)
- `userid`, `user` — editor identity

Two code paths feed revisions to the same `determine_authorship`
method:

- `analyse_article(revisions)` (line 139) — JSON-shaped revisions from
  the MediaWiki Action API.
- `analyse_article_from_xml_dump(page)` (line 62) — `mwxml` Revision
  objects from a dump file.

For our purposes they are equivalent. Pick one entry shape; convert at
the boundary.

## 2. State carried across revisions

The `Wikiwho` object (line 32) carries:

| Field | Purpose |
|-------|---------|
| `paragraphs_ht` | dict {paragraph_hash → [Paragraph, …]} of every paragraph that has appeared in any revision |
| `sentences_ht` | dict {sentence_hash → [Sentence, …]} of every sentence that has appeared in any revision |
| `spam_ids` | list of revision ids flagged as vandalism |
| `spam_hashes` | list of revision SHA-1 hashes flagged as vandalism |
| `tokens` | ordered list of every Word ever introduced (lifetime) |
| `revisions` | dict {rev_id → Revision} of non-spam revisions |
| `ordered_revisions` | list of rev_ids in processing order |
| `rvcontinue` | opaque string for resuming from the WP API |
| `title` | article title |
| `page_id` | article id |
| `token_id` | monotonic counter for the next token id (per article) |
| `revision_curr` | the Revision being processed |
| `revision_prev` | the previous successful Revision |
| `text_curr` | the current revision's text, **lowercased** |

The hash tables grow with the article's history and never shrink. This
is the key memory cost.

## 3. Per-revision flow

For each revision (`analyse_article`, lines 145–205):

### 3.1 Skip cases

- If `texthidden` or `textmissing` is set in the revision dict
  (line 146), skip — do not advance `revision_prev`, do not store.
- For the XML path, if `revision.deleted.text` or
  `revision.deleted.restricted` (line 70), skip.

### 3.2 Hash duplicate check

Compute (or read) the revision SHA-1. If it matches one of
`spam_hashes`, mark this revision as vandalism (line 80).

### 3.3 Length-based vandalism heuristic

If the revision is **not** a good-faith move (`comment AND minor`),
and the previous revision was sizable, and this revision is a large
shrink, flag as vandalism (lines 85–92):

```
if previous.length > 1000
   and current.text_len < 1000
   and (current.text_len - previous.length) / previous.length <= -0.40:
    vandalism = True
```

Constants are module-level (lines 22–28):

```
CHANGE_PERCENTAGE = -0.40
PREVIOUS_LENGTH = 1000
CURR_LENGTH = 1000
UNMATCHED_PARAGRAPH = 0.0
TOKEN_DENSITY_LIMIT = 20
TOKEN_LEN = 100
FLAG = "move"
```

Port these constants exactly.

### 3.4 If vandalism: roll back

Set `revision_curr = revision_prev`, append to `spam_ids` and
`spam_hashes`, **do not call `determine_authorship`**, do not advance
the revision pointer logically.

### 3.5 If not vandalism: process

Build a fresh `Revision()`, populate metadata, **lowercase** the text
(line 123), and call `determine_authorship`. If that detects vandalism
(returns True via `possible_vandalism`), roll back as in 3.4.
Otherwise add to `revisions` and `ordered_revisions`.

### 3.6 Reset

`self.temp = []` at the end of every revision (line 137).

## 4. `determine_authorship` — the matching cascade

Called once per non-spam revision (line 207). It runs three levels in
sequence, each handling what the previous didn't:

```
analyse_paragraphs_in_revision(self)
    → unmatched_paragraphs_curr, unmatched_paragraphs_prev, matched_paragraphs_prev

if unmatched_paragraphs_curr:
    analyse_sentences_in_paragraphs(unmatched_paragraphs_curr, unmatched_paragraphs_prev)
        → unmatched_sentences_curr, unmatched_sentences_prev, matched_sentences_prev, total_sentences

    if unmatched_paragraphs_curr count / current paragraphs > UNMATCHED_PARAGRAPH (0.0):
        possible_vandalism = True   # if any paragraph is unmatched, possible vandalism

    if unmatched_sentences_curr:
        analyse_words_in_sentences(unmatched_sentences_curr, unmatched_sentences_prev, possible_vandalism)
            → matched_words_prev, vandalism
```

After the cascade, **outbound deletion is recorded** for any unmatched
previous word (lines 257–270), unless the revision was flagged as
vandalism.

Then the function **resets `matched` flags** on every paragraph,
sentence, and word it touched (lines 273–305), and **updates `inbound`
and `last_rev_id`** for matched words. The reset has to happen because
`matched` is shared mutable state across revisions; without resetting,
a matched word would not be available to match in a later revision.

This reset is one of the most bug-prone parts of the reference. Our
implementation should not use a shared `matched` flag at all — use
per-iteration sets instead.

### 4.1 Paragraph matching (lines 327–459)

For each non-empty paragraph in the current text:

- Compute MD5 of the paragraph (`calculate_hash` in utils.py).
- Look it up in `revision_prev.paragraphs` (previous-revision hash
  table). If found and unmatched:
  - If no word in any sentence of that paragraph is already matched
    (`matched_one == False`), match the whole paragraph: mark
    paragraph, every sentence, every word as matched. Reuse the
    paragraph reference in the current revision.
  - If all words are already matched (`matched_all == True`), just
    mark the paragraph as matched and continue.
  - Otherwise leave it unmatched.
- If not matched against `revision_prev`, look it up in the global
  `paragraphs_ht` (every paragraph ever seen in this article). Same
  logic. This is what lets a deleted-and-reintroduced paragraph
  inherit all the token ids of its original tokens.
- If still not matched, create a new `Paragraph()` and add it to the
  current revision; it goes into `unmatched_paragraphs_curr` for the
  next stage.

Previous-revision paragraphs that weren't matched become
`unmatched_paragraphs_prev`.

**Quirk to mirror:** if the same paragraph hash appears multiple times
in a previous revision (lines 449–455), the code disambiguates by
counting occurrences via `self.temp`. Our implementation can use a
queue-per-hash instead, which is cleaner.

### 4.2 Sentence matching (lines 461–582)

For each *unmatched* paragraph in the current revision:

- Split into sentences (`split_into_sentences`).
- For each sentence:
  - Trim, skip if empty.
  - Tokenize and rejoin with single spaces (line 479):
    `sentence = ' '.join(split_into_tokens(sentence))`.
    This normalizes whitespace.
  - MD5-hash the normalized sentence.
  - Look up in each `unmatched_paragraph_prev`'s `sentences` dict. If
    found, unmatched, no word already matched → match it (lines
    487–511).
  - Failing that, look up in the global `sentences_ht` (lines 519–550).
  - If still unmatched, create a new `Sentence()` and add it to the
    paragraph; queue it for token-level matching.

Previous-paragraph sentences not matched go into
`unmatched_sentences_prev`, with `matched=True` set on them (lines
578–580) to prevent later iterations from re-matching.

### 4.3 Token-level matching (lines 584–691)

For each *unmatched* sentence pair (current vs. previous):

1. Build `text_prev` — flat list of all unmatched word values in
   `unmatched_sentences_prev`.
2. Build `text_curr` — flat list of all word values in
   `unmatched_sentences_curr`, stored back into each sentence's
   `splitted`.
3. **Vandalism check 1 (line 604):** if `text_curr` is empty, this is
   purely a deletion. Return without changes to current sentences;
   outbound for previous tokens will be set in the caller.
4. **Vandalism check 2 (lines 608–613):** if `possible_vandalism` was
   set (because most paragraphs were new) AND the average frequency of
   tokens in `text_curr` exceeds 20 (`TOKEN_DENSITY_LIMIT`), confirm
   vandalism and stop. (This catches copy-paste vandalism — pasted
   text has anomalously high token repetition.)
   - `compute_avg_word_freq` in utils.py: build a `Counter` over tokens,
     ignore tokens shorter than `TOKEN_LEN` (100 chars), return mean
     count.
5. **Insertion-only case (lines 616–629):** if `text_prev` is empty,
   every current token is new. Create a fresh `Word()` for each, with
   a new `token_id`, `origin_rev_id = current_rev_id`.
6. **General case (lines 631–691):** run `difflib.Differ.compare(text_prev, text_curr)`.
   Walk the resulting diff transcript:
   - `' x'` (matched): pop the first unmatched previous word with
     `value == x`, mark it matched, append it to the current sentence.
   - `'-x'` (deleted): pop the first unmatched previous word with
     `value == x`, mark it matched, append its outbound with the
     current rev id.
   - `'+x'` (added): create a fresh `Word()`, new `token_id`,
     `origin_rev_id = current_rev_id`, append to the current sentence
     and to `self.tokens`.

The "pop the first unmatched word with this value" semantics is
**critical for parity** — when the same word string appears multiple
times in the previous revision, the order in which we consume them
determines which token_id ends up where.

### 4.4 Recording outbound and inbound

After the cascade succeeds (lines 257–305):

- **Outbound** (deletions): for every unmatched previous-revision word
  not already recorded, append the current rev id to its `outbound`.
- **Inbound** (reintroductions): for every matched previous-revision
  word whose `last_rev_id != revision_prev.id` (i.e., it was absent
  from at least one revision and is now back), append the current rev
  id to its `inbound`. Update `last_rev_id` regardless.

### 4.5 Hash table updates

After successful (non-vandalism) processing (lines 308–323):

- New paragraphs go into `paragraphs_ht[hash]` (append).
- New sentences go into `sentences_ht[hash]` (append).
- After insertion, `paragraph.value = ''` and `sentence.value = ''` to
  drop the raw text (lines 314, 322) — they're no longer needed once
  hashed. `sentence.splitted = None` (line 323) for the same reason.

## 5. Tokenization

Defined in `lib/WikiWho/WikiWho/utils.py`. Port these patterns
verbatim; any difference shifts every downstream token id.

| Function | Behavior |
|----------|----------|
| `calculate_hash(text)` | `hashlib.md5(text.encode('utf-8')).hexdigest()` |
| `split_into_paragraphs(text)` | split on `\n\n+` (one or more blank lines); also various MediaWiki paragraph break patterns — see source |
| `split_into_sentences(text)` | split on sentence-ending punctuation followed by whitespace; with MediaWiki-specific tweaks |
| `split_into_tokens(text)` | the actual tokenizer: splits on whitespace AND emits punctuation, wiki-markup characters (`{{}}[]<>=|`), references, etc., as separate tokens |

**Read this file end-to-end before writing the tokenizer in Rust.** It
is short but has years of accumulated edge cases. The regex patterns
are the spec.

The text is **lowercased** before tokenization (`wikiwho.py:123`,
`text_curr = text.lower()`), so the tokenizer sees lowercase input.

## 6. The diff: `difflib.Differ` and what to replace it with

The reference uses `difflib.Differ().compare(text_prev, text_curr)`
where both inputs are lists of token strings. `Differ` returns a
transcript line per output position, prefixed with `'  '`, `'- '`,
`'+ '`, or `'? '` (we ignore `'?'`).

`Differ` is built on `SequenceMatcher` which uses a longest-common-
subsequence variant. It is **not** Myers diff and has different
tie-breaking on ambiguous diffs.

### Recommended replacement

Use **Myers diff** on `&[u32]` token id sequences (after interning).
For the common case (small edits to a long sequence), Myers is O(N + D²)
where D is the edit distance, which beats `Differ`'s O(N·M).

### Parity caveat

When the same token string appears multiple times in both `text_prev`
and `text_curr`, `Differ` and Myers may match them to different
positions. This is observable as different `o_rev_id`s on otherwise-
identical-looking tokens. There are two approaches:

1. **Hope this doesn't matter much.** Run the parity corpus and see
   how many tokens diverge. If <0.1% of tokens differ on average and
   none of the affected revisions are in critical articles for
   downstream consumers, accept the divergence and document it.
2. **Match Differ's tie-breaking exactly.** Differ uses a specific
   "anchor-match" heuristic that prefers matches surrounded by other
   matches. This is specifiable; the CPython source for `SequenceMatcher`
   is the reference. If parity demands it, port this on top of Myers.

Start with (1). Move to (2) only if the parity corpus shows a real
problem.

> **Resolved 2026-05-22:** Myers diff on `&[u32]` interned token ids.
> Threshold for escalating to Differ-compatible tie-breaking: >0.1% of
> tokens diverge across the parity corpus, OR any divergence in the
> known-hard articles (Obama, Trump, COVID-19, Israel–Hamas war,
> Hitler, Jesus, Wikipedia itself).

## 7. Editor identity

The `editor` field on a Revision (used in output JSON) is a string:

- For registered users: `str(user_id)`
- For anons / unregistered: `'0|<name>'` where `<name>` is the IP or
  username string (note the leading `'0|'` sentinel)
- For missing identity: `''`

Logic in `wikiwho.py:107–120` and `wikiwho.py:182–188`.

The whocolor pipeline (`wikiwho/wikiwho_simple.py:get_whocolor_data`)
treats anon editors as a hash for CSS class purposes (an MD5 of the
`'0|name'` string) so anon edits stay color-grouped without exposing
identifying details.

## 8. Output shapes

See [API.md](API.md) for the response builders
(`get_revision_content`, `get_revision_min_content`, etc.) and the
exact JSON schemas the downstream consumers expect.

## 9. Quirks to mirror (the "did you know" list)

These are non-obvious behaviors that *will* break parity if missed:

1. **Text is lowercased** before tokenization. So all token strings
   in the output JSON are lowercase. Yes, even proper nouns.
2. **Empty paragraphs and empty sentences are skipped** (lines 340,
   476). They don't get hashed; they don't go into the hash tables.
3. **Whitespace in sentences is normalized** before hashing (line 479)
   — sentence content goes through `split_into_tokens` and is rejoined
   with single spaces. So `"foo  bar"` and `"foo bar"` have the same
   sentence hash. Paragraph hashing does NOT do this (the source has
   a TODO at line 343 wondering whether it should).
4. **`temp` counter trick** for duplicate paragraph/sentence hashes
   in the previous revision (lines 449–455, 569–573). When the same
   hash appears N times, the Nth occurrence is selected on the Nth
   lookup. Our implementation should use a queue/iterator instead.
5. **Vandalism rollback returns `revision_curr = revision_prev`**
   (lines 96, 130, 172, 196). After this, the next revision will see
   `revision_prev = whatever_was_two_revisions_ago_or_the_last_good_one`.
6. **`RecursionError` is caught** and logged to a DB table
   (`wikiwho_api/api/handler.py:532–550`). Python sets recursion limit
   to 5000 in `handler.py:37`. The actual recursion in the algorithm
   comes from… honestly we don't see it directly in the code — it
   must come from deep dict/list nesting during pickle load on huge
   articles. The Rust rewrite shouldn't hit this.
7. **Hash collisions are not handled.** The reference uses MD5 and
   assumes no collisions across paragraphs or sentences of an article.
   In practice this is fine; an article would need to contain two
   paragraphs that MD5-collide for it to matter. Mirror this
   assumption.
8. **`spam_hashes` is checked first, before the heuristic.** A revision
   with the same SHA-1 as a known-spam revision is automatically spam,
   no further checks. This is how repeated identical vandalism gets
   filtered cheaply.
9. **The hash of an empty string is included** in `paragraphs_ht` if
   the very first revision has empty paragraphs — except it doesn't,
   because empty paragraphs are skipped. Just don't worry about this.
10. **`page.namespace == 0`** is the only namespace processed. Anything
    else is rejected upstream in `WPHandler.handle()`. For the rewrite,
    enforce the same constraint at the API boundary.

## 10. Parity test methodology

Two levels of parity check:

### Level A: token-by-token JSON match

For each fixture `(lang, page_id, rev_id)`, compare the rewrite's
output for `rev_content/rev_id/{rev_id}/?o_rev_id=true&editor=true&token_id=true&in=true&out=true`
against the snapshot. The two must agree on:

- `article_title`, `page_id`, `success`
- For each token in the `tokens` array, in order:
  - `str`
  - `o_rev_id`
  - `editor`
  - `token_id` ← this is the strict test; if token ids differ, the
    rewrite is processing revisions in a different order or with a
    different splitter
  - `in` (list of rev_ids)
  - `out` (list of rev_ids)

### Level B: lifetime equivalence

For an article with N revisions, process revisions 1..k for each
1 ≤ k ≤ N and check that the result at revision k matches the
reference's snapshot at the same point. (Don't actually do this for
every k for every article — sample.) This catches algorithm-state
divergences that only manifest after many revisions.

If level A passes, level B will almost certainly pass too; level B
is the long-tail correctness check.

### Acceptable divergences (documented exceptions)

- **Diff tie-breaking** on duplicate tokens. Document the rate of
  divergence. If it exceeds 0.1% of tokens overall, escalate to
  implementing Differ-compatible tie-breaking (§6).

### Unacceptable divergences

- Different token strings.
- Different `editor` values.
- Different total token counts in a revision.
- Missing or extra revisions in the output.
- Different paragraph or sentence boundaries (would show up as
  different token ordering).

## 11. Things the algorithm does NOT do

For completeness, things the reference is sometimes mistakenly
believed to do but actually doesn't:

- It does not preserve the original whitespace of the wikitext —
  tokens are stored as their lowercased values, separated by spaces
  in output.
- It does not track edits at the character level — only token level.
- It does not handle the wikitext semantically; it treats brackets,
  pipes, equals signs, etc., as token characters that happen to split.
- It does not detect moves of text within a revision — a moved
  paragraph reads as "deleted here" + "reintroduced there" with the
  same token ids (because the same paragraph hash matches via
  `paragraphs_ht`).
- It does not deduplicate identical tokens within a single revision —
  two occurrences of "the" are two `Word` objects with different
  `token_id`s.

## References to the reference implementation

| Concept | File | Lines |
|---------|------|-------|
| Top-level state | `wikiwho.py` | 32–53 |
| `analyse_article` (JSON path) | `wikiwho.py` | 139–205 |
| `analyse_article_from_xml_dump` | `wikiwho.py` | 62–137 |
| `determine_authorship` | `wikiwho.py` | 207–325 |
| `analyse_paragraphs_in_revision` | `wikiwho.py` | 327–459 |
| `analyse_sentences_in_paragraphs` | `wikiwho.py` | 461–582 |
| `analyse_words_in_sentences` | `wikiwho.py` | 584–691 |
| Tokenization | `utils.py` | (entire) |
| Data classes | `structures.py` | (entire) |
| Response builders | `wikiwho_simple.py` | (entire) |
| Spam constants | `wikiwho.py` | 22–29 |
| Recursion limit | `api/handler.py` | 37 |
| Pickle locking | `api/utils_pickles.py` | 31–64 |
