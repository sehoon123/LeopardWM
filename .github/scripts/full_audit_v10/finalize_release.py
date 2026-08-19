from pathlib import Path
import hashlib
import json
import os
import subprocess

version = os.environ['RELEASE_VERSION']
tag = os.environ['RELEASE_TAG']
zip_name = f'LeopardWM-{version}-x86_64-windows.zip'
zip_hash = hashlib.sha256(Path(zip_name).read_bytes()).hexdigest()

manifest_path = Path('dist/scoop/leopardwm.json')
manifest = json.loads(manifest_path.read_text(encoding='utf-8'))
manifest['version'] = version
manifest['architecture']['64bit']['url'] = (
    f'https://github.com/sehoon123/LeopardWM/releases/download/{tag}/{zip_name}'
)
manifest['architecture']['64bit']['hash'] = zip_hash
manifest['extract_dir'] = f'LeopardWM-{version}-x86_64-windows'
manifest_path.write_text(json.dumps(manifest, indent=4) + '\n', encoding='utf-8')

changed = subprocess.check_output(['git', 'diff', '--name-only'], text=True).splitlines()
report = f'''# LeopardWM {tag} full repository audit

## Scope

- Audited Rust, TOML, embedded HTML/CSS/JavaScript, GitHub Actions, PowerShell, WiX, and distribution metadata.
- Reviewed daemon state transitions, Win32 resource ownership, IPC startup/security, persistence, animation, layout/monitor isolation, Settings UI, CLI, watchdog, update delivery, and packaging.

## Hardened areas

- Independent fork identity and exact custom release version reporting.
- Numeric custom-release update comparison and bounded/cancellable network reads.
- Serialized Settings saves, visible update preference, unknown-field preservation, responsive layout, and Edge GUI smoke coverage.
- Shared crash-safe atomic file writes for daemon and CLI.
- IPC first-instance startup handshake and aligned token-security buffers.
- Correct Win32 `GetMessageW` error handling and safer worker startup.
- Thumbnail/raw-handle ownership cleanup and taskbar worker readiness.
- Floating-window lifecycle cleanup, stale-border removal, taskbar policy, monitor-isolated UI geometry, and transition consistency.
- Scoop/MSI/ZIP distribution metadata for `sehoon123/LeopardWM`.

## Verification gates

- `cargo fmt --check` and `git diff --check`
- all-target debug tests
- all-target Clippy with warnings denied
- all-target Cargo check
- optimized library/binary/integration tests
- repeated high-risk regression filters
- Microsoft Edge Settings GUI smoke tests at 640x420 and 780x560
- optimized Windows build and GUI subsystem inspection
- MSI administrative installation and binary identity checks
- published ZIP/MSI redownload and integrity verification

## Source change set

{len(changed)} files before this report and finalized manifest.
'''
Path('agent_docs/full-audit-v0.2.6-sehoon.10.md').write_text(report, encoding='utf-8')
print(f'Scoop ZIP SHA-256: {zip_hash}')
