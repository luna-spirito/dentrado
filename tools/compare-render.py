#!/usr/bin/env python3
"""Compare kolorinko's #page-content against Wikidot's golden HTML.

Usage: compare.py <golden_file|URL> <kolorinko_page_arg>
  e.g. compare.py http://rpcauthority.wikidot.com/rpc-001-j rpcauthority/rpc-001-j

Normalizes whitespace, strips Leptos `<!>` markers, and prints a unified diff
of the normalized #page-content regions.
"""
import re, sys, subprocess, urllib.request, os

def norm_html(s: str) -> str:
    # Leptos SSR emits "<!>" for empty fragments; remove them.
    s = s.replace('<!>', '')
    # collapse whitespace
    s = re.sub(r'\s+', ' ', s)
    # insert newlines before block-level tags for readable diffs
    s = re.sub(r'(</?(?:p|div|blockquote|table|tbody|tr|td|th|ul|ol|li|h[1-6]|hr|br|span|img|a|em|strong|sup|sub)\b)', r'\n\1', s)
    # de-blank lines
    s = re.sub(r'\n[ \t]*', '\n', s)
    return s.strip()

def extract_page_content(html: str) -> str:
    i = html.find('id="page-content"')
    if i < 0: return "(no #page-content found)"
    start = html.rfind('<div', 0, i)
    depth = pos = 0; end = len(html); p = start
    while p < len(html):
        o = html.find('<div', p); c = html.find('</div>', p)
        if o == -1 and c == -1: break
        if o != -1 and (c == -1 or o < c):
            depth += 1; p = o + 4
        else:
            depth -= 1
            if depth == 0: end = c + 6; break
            p = c + 6
    return html[start:end]

def get_golden(src: str) -> str:
    if src.startswith('http'):
        req = urllib.request.Request(src, headers={'User-Agent':'Mozilla/5.0'})
        html = urllib.request.urlopen(req, timeout=30).read().decode('utf-8','replace')
    else:
        html = open(src, encoding='utf-8', errors='replace').read()
    return extract_page_content(html)

def get_kolorinko(page: str) -> str:
    cfg = 'apps/kolorinko/config.dev.toml'
    out = subprocess.run(['cargo','run','-p','kolorinko','--quiet','--',cfg,'render',page],
                         capture_output=True, text=True)
    if out.returncode != 0:
        sys.stderr.write(out.stderr); sys.exit(1)
    return extract_page_content(out.stdout)

golden_src, page = sys.argv[1], sys.argv[2]
g = norm_html(get_golden(golden_src))
k = norm_html(get_kolorinko(page))
open('/tmp/g.norm','w').write(g); open('/tmp/k.norm','w').write(k)
# unified diff
import difflib
for line in difflib.unified_diff(g.splitlines(), k.splitlines(),
                                 fromfile='golden', tofile='kolorinko', lineterm=''):
    print(line)
