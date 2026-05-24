"""Compare production WhoColor response to ours for Icaro.

The previous session flagged Icaro as PREVIEW_WARN+UNKNOWN_PARAM in
*our* HTML. Question: does *production's* HTML have the same warning?
If yes, this isn't a divergence — it's a parity-preserving artifact
of the reference implementation (which we ported).

If no, our parser is doing something different from production's.
"""
import json
import re
import sys
import urllib.parse
import urllib.request


def fetch_json(url, timeout=180):
    req = urllib.request.Request(url, headers={'User-Agent': 'wikiwho-rs-parity (sage@wikiedu.org)'})
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read().decode('utf-8'))


def whocolor_url(base, lang, title):
    return (f"{base}/{lang}/whocolor/v1.0.0-beta/"
            f"{urllib.parse.quote(title, safe='')}/0/?origin=*")


def main():
    lang = 'en'
    title = 'Icaro'
    prod = fetch_json(whocolor_url('https://wikiwho-api.wmcloud.org', lang, title))
    ours = fetch_json(whocolor_url('https://wikiwho-rs.wmcloud.org', lang, title))

    if not prod.get('success'):
        print(f'prod success=false: {prod}')
        return 1
    if not ours.get('success'):
        print(f'ours success=false: {ours}')
        return 1

    prod_html = prod.get('extended_html', '')
    our_html = ours.get('extended_html', '')

    print(f'prod html bytes: {len(prod_html)}')
    print(f'ours html bytes: {len(our_html)}')
    print(f'prod has "Preview warning": {("Preview warning" in prod_html)}')
    print(f'ours has "Preview warning": {("Preview warning" in our_html)}')
    print(f'prod has "unknown parameter": {("unknown parameter" in prod_html)}')
    print(f'ours has "unknown parameter": {("unknown parameter" in our_html)}')

    # Save for offline diff
    with open('/tmp/icaro_prod.html', 'w') as f:
        f.write(prod_html)
    with open('/tmp/icaro_ours.html', 'w') as f:
        f.write(our_html)
    print('wrote /tmp/icaro_prod.html and /tmp/icaro_ours.html')

    # Try to locate the warning region in each.
    for name, html in (('prod', prod_html), ('ours', our_html)):
        idx = html.find('Preview warning')
        if idx >= 0:
            print(f'--- {name}: Preview warning context ({idx-100}..{idx+200}) ---')
            print(html[max(0, idx-100):idx+200])
            print('---')


if __name__ == '__main__':
    sys.exit(main() or 0)
