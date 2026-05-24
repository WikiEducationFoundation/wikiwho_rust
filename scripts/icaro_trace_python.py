"""Patch WhoColor.WikiMarkupParser with print-traces and replay
on Icaro to find the EXACT reason the outer `}}` of `{{multiple
issues...}}` gets a span wrapped around it.

Strategy: monkey-patch __parse_wiki_text, __get_next_special_element,
__get_special_elem_end, __add_spans to print every step that touches
the multiple-issues region (bytes 44..144 in the wikitext).
"""
import json
import re
import sys
import io

sys.path.insert(0, '/home/sage/play/wikiwho_api/env/lib/python3.9/site-packages')
from WhoColor.parser import WikiMarkupParser
from WhoColor.special_markups import SPECIAL_MARKUPS, REGEX_HELPER_PATTERN

# Region we care about (in pre-substitution wikitext bytes)
REGION = (44, 144)


def _in_region(pos):
    return REGION[0] <= pos <= REGION[1]


def main():
    wt = open('/tmp/icaro.wt').read()
    toks_raw = json.load(open('/tmp/icaro_tokens.json'))
    toks = [
        {
            'str': t['str'],
            'editor': t['editor'],
            'editor_name': t['editor'],
            'class_name': t['class_name'],
            'conflict_score': 0,
        }
        for t in toks_raw
    ]
    p = WikiMarkupParser(wt, toks)

    # Apply newline substitution like generate_extended_wiki_markup() does,
    # but inline so we can call __parse() directly.
    p.wiki_text = p.wiki_text.replace('\r\n', REGEX_HELPER_PATTERN).replace('\n', REGEX_HELPER_PATTERN).replace('\r', REGEX_HELPER_PATTERN)

    # Patch __parse_wiki_text to log iteration state in region.
    orig_parse = type(p)._WikiMarkupParser__parse_wiki_text
    orig_next = type(p)._WikiMarkupParser__get_next_special_element
    orig_end = type(p)._WikiMarkupParser__get_special_elem_end
    orig_add = type(p)._WikiMarkupParser__add_spans
    orig_settok = type(p)._WikiMarkupParser__set_token

    depth = [0]

    def trace(msg):
        indent = '  ' * depth[0]
        print(f'{indent}{msg}', flush=True)

    def __set_token(self):
        orig_settok(self)
        if self.token and _in_region(self._wiki_text_pos):
            trace(f'set_token: idx={self._token_index} str={self.token["str"]!r} end={self.token["end"]} pos={self._wiki_text_pos}')
    type(p)._WikiMarkupParser__set_token = __set_token

    def __get_next_special_element(self):
        out = orig_next(self)
        if _in_region(self._wiki_text_pos):
            if out:
                trace(f'next_special: start={out.get("start")} type={out.get("type")} no_spans={out.get("no_spans")} (from pos={self._wiki_text_pos})')
            else:
                trace(f'next_special: NONE (from pos={self._wiki_text_pos})')
        return out
    type(p)._WikiMarkupParser__get_next_special_element = __get_next_special_element

    def __get_special_elem_end(self, special_elem):
        out = orig_end(self, special_elem)
        if special_elem and _in_region(special_elem.get('start', 0)):
            trace(f'special_elem_end({special_elem.get("start")}->end_re={special_elem.get("end_regex").pattern if special_elem.get("end_regex") else None}): {out} (from pos={self._wiki_text_pos})')
        return out
    type(p)._WikiMarkupParser__get_special_elem_end = __get_special_elem_end

    def __add_spans(self, token, new_span=True):
        if _in_region(self._wiki_text_pos):
            trace(f'add_spans(new_span={new_span}, open_span_before={self._open_span}) at pos={self._wiki_text_pos} tok_idx={self._token_index} tok={token["str"]!r}')
        orig_add(self, token, new_span)
    type(p)._WikiMarkupParser__add_spans = __add_spans

    def __parse_wiki_text(self, add_spans=True, special_elem=None, no_jump=False):
        if _in_region(self._wiki_text_pos) or (special_elem and _in_region(special_elem.get('start', 0))):
            depth[0] += 1
            se_info = ''
            if special_elem:
                se_info = f' special_elem.start={special_elem.get("start")} type={special_elem.get("type")} no_spans={special_elem.get("no_spans")}'
            trace(f'ENTER parse_wiki_text(add_spans={add_spans}, no_jump={no_jump}){se_info} at pos={self._wiki_text_pos} jumped={sorted(self._jumped_elems)}')
            ret = orig_parse(self, add_spans, special_elem, no_jump)
            trace(f'EXIT  parse_wiki_text -> pos={self._wiki_text_pos}')
            depth[0] -= 1
            return ret
        return orig_parse(self, add_spans, special_elem, no_jump)
    type(p)._WikiMarkupParser__parse_wiki_text = __parse_wiki_text

    # Run.
    p._WikiMarkupParser__parse()


if __name__ == '__main__':
    main()
