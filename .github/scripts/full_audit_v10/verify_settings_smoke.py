from pathlib import Path
import html
import re
import sys

for dump_path in map(Path, sys.argv[1:]):
    text = dump_path.read_text(encoding='utf-8', errors='replace')
    match = re.search(
        r'<pre id="settings-smoke-result">([\s\S]*?)</pre>',
        text,
        flags=re.I,
    )
    if not match:
        raise SystemExit(f'{dump_path}: Settings smoke result element is missing')
    result = html.unescape(match.group(1)).strip()
    print(f'[{dump_path.name}]\n{result}', flush=True)
    if result != 'PASS':
        raise SystemExit(f'{dump_path}: Settings smoke failed:\n{result}')
print('Settings GUI smoke tests passed')
