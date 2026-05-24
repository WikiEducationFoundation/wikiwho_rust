"""Verify what the Python WhoColor parser does for two test inputs:

1. The Curzon-style synthetic: `{{Infobox |a = {{nest |[[L1]] |[[L2]]}}|b = end}}`
   — single-line; our regression test asserts no spans inside the outer template.

2. The Icaro-style synthetic: same shape but with newlines between inner templates.

If Python emits a span inside the outer template, that confirms the
behavior we observe is parity-preserving rather than a bug.
"""
import sys

sys.path.insert(0, '/home/sage/play/wikiwho_api/env/lib/python3.9/site-packages')
from WhoColor.parser import WikiMarkupParser


def mk(s):
    return {
        'str': s,
        'editor': '1',
        'editor_name': '1',
        'class_name': '1',
        'conflict_score': 0,
    }


def run(name, wt, str_list):
    toks = [mk(s) for s in str_list]
    p = WikiMarkupParser(wt, toks)
    p.generate_extended_wiki_markup()
    out = p.extended_wiki_text
    print(f'=== {name} ===')
    print(f'wt: {wt!r}')
    print(f'out: {out!r}')
    print()


# Case 1: Curzon-style synthetic.
run(
    'curzon-synthetic (no newlines)',
    '{{Infobox |a = {{nest |[[L1]] |[[L2]]}}|b = end}}',
    ['{{', 'infobox', '|', 'a', '=', '{{', 'nest', '|', '[[', 'l1', ']]', '|', '[[', 'l2', ']]', '}}', '|', 'b', '=', 'end', '}}'],
)

# Case 2: Icaro-style synthetic with newlines.
run(
    'icaro-synthetic (newlines between inner templates)',
    '{{outer|{{inner-a|x=1}}\n{{inner-b|y=2}}}}',
    ['{{', 'outer', '|', '{{', 'inner-a', '|', 'x', '=', '1', '}}', '{{', 'inner-b', '|', 'y', '=', '2', '}}', '}}'],
)
