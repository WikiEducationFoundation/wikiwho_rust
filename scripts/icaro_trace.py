"""Trace tool for the Icaro template-bleed bug.

Loads /tmp/icaro.wt + /tmp/icaro_tokens.json and prints token info
around the {{multiple issues}} region so we can correlate with the
parser's behavior.

Also runs the real Python WhoColor.WikiMarkupParser on the same
input to dump the (correct) expected output. Difference vs our
Rust output tells us where we diverge.
"""
import json
import sys


def main():
    wt = open('/tmp/icaro.wt').read()
    toks = json.load(open('/tmp/icaro_tokens.json'))
    print(f'wikitext bytes: {len(wt)}')
    print(f'tokens: {len(toks)}')

    # Find the {{multiple issues region in wikitext.
    mi = wt.find('{{multiple issues')
    other = wt.find('{{other')
    print(f'{{{{multiple issues starts at byte {mi}')
    print(f'{{{{other starts at byte {other}')
    print('--- multi-issues region (bytes %d..%d) ---' % (mi, other))
    region = wt[mi:other]
    for i, b in enumerate(region.encode('utf-8')):
        # show absolute byte position; mark { } | \n
        c = chr(b)
        if c in '{}|\n=':
            print(f'  byte {mi+i:4d} = {c!r}')

    # Locate the tokens whose str matches a `{{` or `}}` in this region.
    print('--- token list (first 50) ---')
    for i, t in enumerate(toks[:50]):
        s = t['str']
        print(f'  tok {i:3d}: {s!r:30s} editor={t["editor"]:>16s}')

    # Run the Python WhoColor parser on this input. Need to fake the
    # extra fields it expects.
    sys.path.insert(0, '/home/sage/play/wikiwho_api/env/lib/python3.9/site-packages')
    from WhoColor.parser import WikiMarkupParser
    fake_toks = []
    for t in toks:
        fake_toks.append({
            'str': t['str'],
            'editor': t['editor'],
            'editor_name': t['editor'],
            'class_name': t['class_name'],
            'conflict_score': 0,
        })
    p = WikiMarkupParser(wt, fake_toks)
    p.generate_extended_wiki_markup()
    out = p.extended_wiki_text
    with open('/tmp/icaro_modified_python.wt', 'w') as f:
        f.write(out)
    print(f'python parser wrote /tmp/icaro_modified_python.wt ({len(out)} bytes)')

    # Show the multi-issues region in the Python output.
    py_mi = out.find('{{multiple issues')
    py_other = out.find('{{other')
    print(f'--- python multi-issues region (bytes {py_mi}..{py_other}) ---')
    print(out[py_mi:py_other])

    # Also show the rust output if we have it.
    try:
        rust = open('/tmp/icaro_modified.wt').read()
        rust_mi = rust.find('{{multiple issues')
        rust_other = rust.find('{{other')
        print(f'--- rust multi-issues region (bytes {rust_mi}..{rust_other}) ---')
        print(rust[rust_mi:rust_other])
    except FileNotFoundError:
        pass


if __name__ == '__main__':
    main()
