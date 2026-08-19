from pathlib import Path
import base64
import gzip
import hashlib
import os

parts_dir = Path('../control/.github/patches/full-audit-v10')
parts = sorted(parts_dir.glob('part-*.b64'))
expected_names = [f'part-{index:02}.b64' for index in range(4)]
if [part.name for part in parts] != expected_names:
    raise SystemExit(f'Unexpected patch parts: {[part.name for part in parts]}')

# Each repository file is an independently padded base64 block. Decode the
# blocks separately, then concatenate the original gzip bytes. Concatenating
# the encoded text first is invalid because padding may appear before EOF.
compressed = b''.join(
    base64.b64decode(part.read_text(encoding='ascii').strip(), validate=True)
    for part in parts
)
compressed_hash = hashlib.sha256(compressed).hexdigest()
if compressed_hash != os.environ['PATCH_GZIP_SHA256']:
    raise SystemExit(f'Compressed patch digest mismatch: {compressed_hash}')

patch = gzip.decompress(compressed)
patch_hash = hashlib.sha256(patch).hexdigest()
if patch_hash != os.environ['PATCH_SHA256']:
    raise SystemExit(f'Patch digest mismatch: {patch_hash}')

Path('../full-audit-v10.patch').write_bytes(patch)
print(f'Authenticated {len(patch):,}-byte patch')
