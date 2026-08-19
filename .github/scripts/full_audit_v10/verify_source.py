from pathlib import Path
import re
import subprocess

EXPECTED_FILES = {
    'CONTRIBUTING.md', 'Cargo.toml', 'SECURITY.md',
    'agent_docs/distribution_setup.md', 'agent_docs/release.md',
    'crates/cli/src/args.rs', 'crates/cli/src/config_cmds.rs', 'crates/cli/src/doctor.rs',
    'crates/core_layout/src/tests.rs', 'crates/core_layout/src/workspace/mod.rs',
    'crates/core_layout/src/workspace/operations.rs',
    'crates/daemon/src/atomic_file.rs',
    'crates/daemon/src/command_handler.rs', 'crates/daemon/src/config.rs',
    'crates/daemon/src/event_handler.rs', 'crates/daemon/src/full_audit_tests.rs',
    'crates/daemon/src/helpers.rs', 'crates/daemon/src/ipc_server.rs',
    'crates/daemon/src/layout_apply.rs', 'crates/daemon/src/layout_apply_edge_tests.rs',
    'crates/daemon/src/main.rs', 'crates/daemon/src/notify.rs',
    'crates/daemon/src/persistence.rs', 'crates/daemon/src/settings/html.rs',
    'crates/daemon/src/settings/mod.rs', 'crates/daemon/src/settings/settings.html',
    'crates/daemon/src/settings/win32.rs', 'crates/daemon/src/startup.rs',
    'crates/daemon/src/state.rs', 'crates/daemon/src/transitions.rs',
    'crates/daemon/src/tray.rs', 'crates/daemon/src/ui_sync.rs',
    'crates/daemon/src/update_check.rs', 'crates/ipc/Cargo.toml',
    'crates/ipc/src/atomic_file.rs', 'crates/ipc/src/config_template.rs',
    'crates/ipc/src/distribution.rs', 'crates/ipc/src/lib.rs',
    'crates/platform_win32/src/border.rs', 'crates/platform_win32/src/dialog.rs',
    'crates/platform_win32/src/hotkeys.rs', 'crates/platform_win32/src/ipc_security.rs',
    'crates/platform_win32/src/overlay.rs', 'crates/platform_win32/src/overview.rs',
    'crates/platform_win32/src/tab_strip.rs', 'crates/platform_win32/src/taskbar.rs',
    'crates/platform_win32/src/thumbnail.rs', 'crates/platform_win32/src/toast.rs',
    'crates/watchdog/src/main.rs', 'dist/scoop/leopardwm.json', 'wix/main.wxs',
}

# `git diff --name-only` omits new untracked files, which made the verifier
# blind to exactly the modules it was intended to authenticate. Porcelain
# status reports modifications, deletions, and additions in one stable format.
changed = {
    line[3:]
    for line in subprocess.check_output(
        ['git', 'status', '--porcelain=v1', '--untracked-files=all'],
        text=True,
    ).splitlines()
    if len(line) >= 4
}
if changed != EXPECTED_FILES:
    raise SystemExit(
        f'Change-set mismatch; missing={sorted(EXPECTED_FILES - changed)}, '
        f'extra={sorted(changed - EXPECTED_FILES)}'
    )
if Path('crates/daemon/src/atomic_file.rs').exists():
    raise SystemExit('Obsolete daemon-local atomic writer still exists')

settings = Path('crates/daemon/src/settings/settings.html').read_text(encoding='utf-8')
updater = Path('crates/daemon/src/update_check.rs').read_text(encoding='utf-8')
distribution = Path('crates/ipc/src/distribution.rs').read_text(encoding='utf-8')
combined = '\n'.join(
    path.read_text(encoding='utf-8', errors='replace')
    for path in Path('crates').rglob('*.rs')
)
corpus = '\n'.join([combined, settings, updater, distribution])

required = {
    'independent repository identity': 'https://github.com/sehoon123/LeopardWM',
    'exact release-tag embedding': 'option_env!("LEOPARDWM_RELEASE_TAG")',
    'numeric fork revision parser': 'rsplit_once("-sehoon.")',
    'bounded release response': 'MAX_RELEASE_RESPONSE_BYTES',
    'IPC startup handshake': 'ipc_startup_rx',
    'first-pipe startup failure': 'Failed to create the first IPC pipe instance',
    'visible update preference': 'id="behavior-check_for_updates"',
    'settings save serialization': 'SettingsEvent::SaveRequested',
    'unknown TOML preservation': 'merge_unknown_config_fields',
    'stale-border cleanup': 'clear_tracked_focus',
    'shared monitor isolation': 'isolate_workspace_placements',
    'taskbar readiness handshake': 'ready_rx.recv_timeout',
    'aligned TOKEN_USER buffer': 'Vec<usize>',
    'shared atomic persistence': 'pub mod atomic_file;',
    'full-audit tests': 'mod full_audit_tests;',
}
missing = [name for name, marker in required.items() if marker not in corpus]
if missing:
    raise SystemExit('Missing audit contracts: ' + ', '.join(missing))

forbidden = {
    'upstream update API': 'api.github.com/repos/jcardama/LeopardWM',
    'upstream watchdog AUMID': 'jcardama.LeopardWM.Watchdog',
    'thumbnail handle leak': 'mem::forget(self)',
    'GetMessage bool collapse': 'GetMessageW(&mut msg, None, 0, 0).as_bool()',
    'animation worker startup panic': 'expect("Failed to spawn animation worker")',
    'hidden update preference save': 'check_for_updates: window._initConfig.behavior.check_for_updates',
}
present = [name for name, marker in forbidden.items() if marker in corpus]
if present:
    raise SystemExit('Forbidden stale patterns remain: ' + ', '.join(present))

ids = re.findall(r'id="([^"]+)"', settings)
duplicates = sorted({identifier for identifier in ids if ids.count(identifier) > 1})
if duplicates:
    raise SystemExit(f'Duplicate Settings DOM ids: {duplicates}')

scripts = re.findall(r'<script[^>]*>(.*?)</script>', settings, flags=re.S | re.I)
if len(scripts) != 1:
    raise SystemExit(f'Expected one embedded Settings script, found {len(scripts)}')
Path('../settings-script-v10.js').write_text(scripts[0], encoding='utf-8')
print(f'Exact {len(changed)}-file change set and static contracts verified')
