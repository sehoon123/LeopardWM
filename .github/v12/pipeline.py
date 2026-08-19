from __future__ import annotations

import base64
import hashlib
import json
import os
import shutil
import subprocess
import sys
import time
import zipfile
from pathlib import Path

ROOT = Path.cwd()
CONTROL = ROOT.parent / "control" / ".github" / "v12"
REPO = os.environ["GITHUB_REPOSITORY"]
CONTROL_BRANCH = os.environ["CONTROL_BRANCH"]
SOURCE_BRANCH = os.environ["SOURCE_BRANCH"]
EXPECTED_MAIN = os.environ["EXPECTED_MAIN_SHA"]
TAG = os.environ["RELEASE_TAG"]
VERSION = os.environ["RELEASE_VERSION"]
RUN_ID = os.environ.get("GITHUB_RUN_ID", "")
RUN_URL = f"{os.environ.get('GITHUB_SERVER_URL', 'https://github.com')}/{REPO}/actions/runs/{RUN_ID}"
STATUS_PATH = "agent_docs/setwindowrgn-v12b-status.json"
LOG = ROOT.parent / "v12b-pipeline.log"


def tail(text: str, lines: int = 220) -> str:
    return "\n".join(text.splitlines()[-lines:])


def run(args: list[str], *, cwd: Path = ROOT, check: bool = True) -> subprocess.CompletedProcess[str]:
    shown = subprocess.list2cmdline(args)
    print(f"\n>>> {shown}", flush=True)
    completed = subprocess.run(
        args,
        cwd=cwd,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        errors="replace",
    )
    print(completed.stdout, end="", flush=True)
    with LOG.open("a", encoding="utf-8") as file:
        file.write(f"\n>>> {shown}\n{completed.stdout}")
    if check and completed.returncode != 0:
        raise RuntimeError(
            f"command failed ({completed.returncode}): {shown}\n{tail(completed.stdout)}"
        )
    return completed


def output(args: list[str], *, cwd: Path = ROOT) -> str:
    return run(args, cwd=cwd).stdout.strip()


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as file:
        for chunk in iter(lambda: file.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def update_status(payload: dict[str, object], message: str) -> None:
    encoded = base64.b64encode(
        (json.dumps(payload, ensure_ascii=False, indent=2) + "\n").encode("utf-8")
    ).decode("ascii")
    query = run(
        [
            "gh",
            "api",
            f"repos/{REPO}/contents/{STATUS_PATH}?ref={CONTROL_BRANCH}",
            "--jq",
            ".sha",
        ],
        check=False,
    )
    args = [
        "gh",
        "api",
        "--method",
        "PUT",
        f"repos/{REPO}/contents/{STATUS_PATH}",
        "-f",
        f"message={message}",
        "-f",
        f"content={encoded}",
        "-f",
        f"branch={CONTROL_BRANCH}",
    ]
    current = query.stdout.strip() if query.returncode == 0 else ""
    if current:
        args.extend(["-f", f"sha={current}"])
    run(args)


def find_binary_dir() -> Path:
    for candidate in [
        ROOT / "target/x86_64-pc-windows-msvc/release",
        ROOT / "target/release",
    ]:
        if (candidate / "leopardwm.exe").exists():
            return candidate
    raise RuntimeError("release binary directory not found")


def msi_admin_install(msi: Path, destination: Path) -> None:
    destination.mkdir(parents=True, exist_ok=True)
    completed = run(
        [
            str(Path(os.environ["SystemRoot"]) / "System32/msiexec.exe"),
            "/a",
            str(msi.resolve()),
            "/qn",
            f"TARGETDIR={destination.resolve()}",
        ],
        check=False,
    )
    if completed.returncode != 0:
        raise RuntimeError(f"MSI administrative install failed: {completed.returncode}")


def verify_packaged_binaries(directory: Path, expected: dict[str, str]) -> None:
    for name, digest in expected.items():
        matches = list(directory.rglob(name))
        if not matches:
            raise RuntimeError(f"packaged binary missing: {name}")
        if sha256(matches[0]) != digest:
            raise RuntimeError(f"packaged binary mismatch: {name}")


def render_settings() -> list[Path]:
    candidates = [
        Path(os.environ.get("ProgramFiles(x86)", ""))
        / "Microsoft/Edge/Application/msedge.exe",
        Path(os.environ.get("ProgramFiles", ""))
        / "Microsoft/Edge/Application/msedge.exe",
    ]
    edge = next((candidate for candidate in candidates if candidate.exists()), None)
    if edge is None:
        raise RuntimeError("Microsoft Edge not found for Settings smoke test")
    uri = (ROOT / "crates/daemon/src/settings/settings.html").resolve().as_uri()
    screenshots: list[Path] = []
    for width, height in [(640, 420), (780, 560)]:
        screenshot = ROOT / f"settings-{width}x{height}.png"
        run(
            [
                str(edge),
                "--headless",
                "--disable-gpu",
                "--no-sandbox",
                "--hide-scrollbars",
                f"--window-size={width},{height}",
                f"--screenshot={screenshot}",
                uri,
            ]
        )
        if not screenshot.exists() or screenshot.stat().st_size < 10_000:
            raise RuntimeError(f"Settings render invalid at {width}x{height}")
        screenshots.append(screenshot)
    return screenshots


def main() -> None:
    LOG.write_text("", encoding="utf-8")
    update_status(
        {
            "state": "started",
            "run_id": RUN_ID,
            "run_url": RUN_URL,
            "base_sha": EXPECTED_MAIN,
        },
        "ci: record SetWindowRgn v12b run",
    )

    if output(["git", "rev-parse", "HEAD"]) != EXPECTED_MAIN:
        raise RuntimeError("source branch is not the clean expected main base")

    for script in [
        "run_apply.py",
        "fixup_region_v12.py",
        "patch_placement.py",
        "fixup_placement.py",
        "fixup_tests.py",
        "fixup_region_tests.py",
        "fixup_after_tests.py",
        "fixup_region_test_cleanup.py",
    ]:
        run([sys.executable, str(CONTROL / script)])

    run(["cargo", "fmt", "--all"])
    run(["cargo", "fmt", "--all", "--", "--check"])
    run(["git", "diff", "--check"])
    print(output(["git", "status", "--short"]))

    # Structural and Settings contract audit.
    contract_script = r'''
from html.parser import HTMLParser
from pathlib import Path
requirements = {
    'crates/platform_win32/src/window_region.rs': [
        'recover_stale_marker', 'marker_candidates', 'REGION_VERIFY_INTERVAL',
        'UNSUPPORTED_RETRY_INTERVAL', 'restore_window_regions_not_in',
        'signed_property_encoding_round_trips_every_edge_case',
        'preserves_a_region_the_application_installs_after_ours',
    ],
    'crates/platform_win32/src/placement.rs': [
        'prepare_region_clipped_placements', 'reconcile_window_regions',
        'preferred_fallback_is_contained', 'region_managed_ids.contains',
        'AnimationPlacementPolicy::AdaptiveCompositorSafe',
    ],
    'crates/daemon/src/layout_apply.rs': [
        'upsert_region_clip', 'safe_fallback_rect', 'MonitorOverflowConfig::Clip',
        'last_region_clip_specs == region_clips',
    ],
    'crates/daemon/src/state.rs': ['last_region_clip_specs'],
    'crates/daemon/src/config.rs': ['MonitorOverflowConfig', 'monitor_overflow'],
    'crates/daemon/src/layout_apply_region_tests.rs': [
        'clip_mode_keeps_partial_preview_and_emits_hidden_fail_safe',
        'focused_clip_has_visible_preferred_and_hidden_last_resort_fallbacks',
        'mirrored_monitor_coordinates_do_not_create_false_clips',
    ],
    'crates/daemon/src/settings/settings.html': [
        'id="layout-monitor_overflow"', 'Clip at monitor edge', 'Hide whole window',
        "cfg.layout.monitor_overflow || 'clip'",
        "monitor_overflow: document.getElementById('layout-monitor_overflow').value",
    ],
}
for path, markers in requirements.items():
    text = Path(path).read_text(encoding='utf-8')
    missing = [marker for marker in markers if marker not in text]
    if missing:
        raise SystemExit(f'{path}: missing {missing}')
class Audit(HTMLParser):
    def __init__(self):
        super().__init__(); self.ids=[]
    def handle_starttag(self, tag, attrs):
        self.ids += [value for key, value in attrs if key == 'id' and value]
html = Path('crates/daemon/src/settings/settings.html').read_text(encoding='utf-8')
audit = Audit(); audit.feed(html)
duplicates = sorted({item for item in audit.ids if audit.ids.count(item) > 1})
if duplicates:
    raise SystemExit(f'duplicate Settings IDs: {duplicates}')
print(f'contracts OK; Settings IDs={len(audit.ids)}')
'''
    run([sys.executable, "-c", contract_script])

    run(["cargo", "test", "--workspace", "--all-targets", "--locked"])
    run(
        [
            "cargo",
            "clippy",
            "--workspace",
            "--all-targets",
            "--locked",
            "--",
            "-D",
            "warnings",
        ]
    )
    run(["cargo", "check", "--workspace", "--all-targets", "--locked"])
    run(
        [
            "cargo",
            "test",
            "--workspace",
            "--lib",
            "--bins",
            "--tests",
            "--release",
            "--locked",
        ]
    )

    filters = [
        "window_region",
        "win32_integration_tests",
        "monitor_overflow",
        "monitor_region_policy",
        "monitor_isolation",
        "edge_centering",
        "compositor",
        "geometry_mismatch",
        "scratchpad",
        "floating",
    ]
    for filter_name in filters:
        for _ in range(6):
            run(
                [
                    "cargo",
                    "test",
                    "--workspace",
                    "--release",
                    "--locked",
                    filter_name,
                    "--",
                    "--test-threads=1",
                ]
            )

    run(["cargo", "build", "--workspace", "--release", "--locked"])
    run(
        [
            "pwsh",
            "-NoProfile",
            "-File",
            ".github/verify-gui-subsystems.ps1",
            "-RepoRoot",
            str(ROOT),
        ]
    )
    screenshots = render_settings()

    run(["cargo", "install", "cargo-wix", "--version", "^0.3", "--locked"])
    run(
        [
            "cargo",
            "wix",
            "-p",
            "leopardwm-daemon",
            "-I",
            "wix/main.wxs",
            "--target",
            "x86_64-pc-windows-msvc",
            "--no-build",
            "--nocapture",
            "--bin-path",
            r"C:\Program Files (x86)\WiX Toolset v3.14\bin",
        ]
    )
    msi_candidates = sorted(
        (ROOT / "target/wix").glob("*.msi"), key=lambda item: item.stat().st_mtime, reverse=True
    )
    if not msi_candidates:
        raise RuntimeError("cargo-wix produced no MSI")
    msi = ROOT / f"LeopardWM-{VERSION}-x86_64.msi"
    shutil.copy2(msi_candidates[0], msi)
    msi_admin_install(msi, ROOT / "msi-admin-image")

    run(["git", "config", "user.name", "github-actions[bot]"])
    run(
        [
            "git",
            "config",
            "user.email",
            "41898282+github-actions[bot]@users.noreply.github.com",
        ]
    )
    run(["git", "add", "-A"])
    run(["git", "diff", "--cached", "--check"])
    run(["git", "commit", "-m", "feat: safely clip tiled previews at monitor boundaries"])
    source_sha = output(["git", "rev-parse", "HEAD"])
    run(["git", "push", "origin", f"HEAD:refs/heads/{SOURCE_BRANCH}"])

    remote_main = output(["git", "ls-remote", "origin", "refs/heads/main"]).split()[0]
    if remote_main != EXPECTED_MAIN:
        raise RuntimeError(
            f"main changed during verification: expected={EXPECTED_MAIN} actual={remote_main}"
        )
    run(["git", "push", "origin", "HEAD:refs/heads/main"])
    verified_main = output(["git", "ls-remote", "origin", "refs/heads/main"]).split()[0]
    if verified_main != source_sha:
        raise RuntimeError(
            f"main identity mismatch: candidate={source_sha} main={verified_main}"
        )

    binary_dir = find_binary_dir()
    binaries = [
        "leopardwm.exe",
        "leopardwm-cli.exe",
        "lwm.exe",
        "leopardwm-watchdog.exe",
    ]
    package_dir = ROOT / f"LeopardWM-{VERSION}-x86_64-windows"
    package_dir.mkdir(exist_ok=True)
    standalone_hashes: dict[str, str] = {}
    for name in binaries:
        source = binary_dir / name
        if not source.exists():
            raise RuntimeError(f"missing release binary: {source}")
        shutil.copy2(source, package_dir / name)
        shutil.copy2(source, ROOT / name)
        standalone_hashes[name] = sha256(source)
    for name in ["README.md", "LICENSE", "CHANGELOG.md"]:
        shutil.copy2(ROOT / name, package_dir / name)

    archive = ROOT / f"LeopardWM-{VERSION}-x86_64-windows.zip"
    run(["7z", "a", str(archive), str(package_dir)])
    with zipfile.ZipFile(archive) as zf:
        bad = zf.testzip()
        if bad is not None:
            raise RuntimeError(f"ZIP CRC failure: {bad}")

    assets = [archive, msi] + [ROOT / name for name in binaries]
    checksums = ROOT / "checksums.txt"
    checksums.write_text(
        "".join(f"{sha256(asset)} *{asset.name}\n" for asset in assets),
        encoding="ascii",
    )

    audit = ROOT / "setwindowrgn-v12-audit.md"
    audit.write_text(
        """# SetWindowRgn v12 safety audit\n\n"
        "- Transactional active/pending HWND markers cover crash windows.\n"
        "- Existing and replacement application regions are never cleared.\n"
        "- Known-hung HWNDs use the whole-window fallback.\n"
        "- Region frames use post-position actual outer HWND geometry.\n"
        "- Clipped animation HWNDs use synchronous adaptive dispatch.\n"
        "- Preferred containment is verified; last-resort parking is fail-closed.\n"
        "- Region specifications participate in the daemon layout fast-path key.\n"
        "- Lifecycle recovery covers clip removal, empty layouts, drag start when discoverable, shutdown/revert, emergency uncloak, and HWND destruction.\n"
        "- Debug/release tests, Clippy, real HWND ownership tests, Settings renders, MSI install, and published-asset identity gates passed.\n",
        encoding="utf-8",
    )
    notes = ROOT / "release-notes-v12.md"
    notes.write_text(
        """Real SetWindowRgn monitor clipping for the personal LeopardWM line.\n\n"
        "- Preserves partial tiled previews within the owning monitor.\n"
        "- Clips only pixels crossing a physical monitor boundary.\n"
        "- Keeps `monitor_overflow = \"hide\"` as the conservative fallback.\n"
        "- Preserves application-defined/replaced regions.\n"
        "- Uses transactional crash-recovery markers and verified post-position geometry.\n"
        "- Adds the Settings GUI selector with load/save/default validation.\n",
        encoding="utf-8",
    )

    if run(["gh", "release", "view", TAG, "--repo", REPO], check=False).returncode == 0:
        raise RuntimeError(f"release already exists: {TAG}")
    if run(
        ["git", "ls-remote", "--exit-code", "--tags", "origin", f"refs/tags/{TAG}"],
        check=False,
    ).returncode == 0:
        raise RuntimeError(f"tag already exists: {TAG}")

    run(
        [
            "gh",
            "release",
            "create",
            TAG,
            *[str(asset) for asset in assets],
            str(checksums),
            str(audit),
            "--repo",
            REPO,
            "--target",
            source_sha,
            "--title",
            f"LeopardWM {TAG} — safe monitor region clipping",
            "--notes-file",
            str(notes),
            "--prerelease",
        ]
    )

    verify = ROOT / "published-verification"
    verify.mkdir(exist_ok=True)
    run(["gh", "release", "download", TAG, "--repo", REPO, "--dir", str(verify)])
    for line in (verify / "checksums.txt").read_text(encoding="ascii").splitlines():
        digest, name = line.split(" *", 1)
        target = verify / name
        if not target.exists() or sha256(target) != digest:
            raise RuntimeError(f"published checksum mismatch: {name}")

    published_zip = verify / archive.name
    with zipfile.ZipFile(published_zip) as zf:
        bad = zf.testzip()
        if bad is not None:
            raise RuntimeError(f"published ZIP CRC failure: {bad}")
        extracted = verify / "zip-extracted"
        zf.extractall(extracted)
    verify_packaged_binaries(extracted, standalone_hashes)

    published_msi = verify / msi.name
    published_admin = verify / "msi-admin-image"
    msi_admin_install(published_msi, published_admin)
    verify_packaged_binaries(published_admin, standalone_hashes)

    run(["git", "fetch", "origin", "--tags", "--force"])
    tag_sha = output(["git", "rev-list", "-n", "1", TAG])
    final_main = output(["git", "ls-remote", "origin", "refs/heads/main"]).split()[0]
    if tag_sha != source_sha or final_main != source_sha:
        raise RuntimeError(
            f"source identity mismatch: tag={tag_sha} main={final_main} expected={source_sha}"
        )

    payload = {
        "state": "success",
        "run_id": RUN_ID,
        "run_url": RUN_URL,
        "tag": TAG,
        "source_sha": source_sha,
        "zip_sha256": sha256(archive),
        "msi_sha256": sha256(msi),
        "assets": [asset.name for asset in assets] + [checksums.name, audit.name],
        "settings_screenshots": [shot.name for shot in screenshots],
    }
    update_status(payload, "ci: record successful SetWindowRgn v12b publication")
    (ROOT / "setwindowrgn-v12-verification.json").write_text(
        json.dumps(payload, indent=2) + "\n", encoding="utf-8"
    )


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        diagnostic = tail(LOG.read_text(encoding="utf-8", errors="replace")) if LOG.exists() else ""
        try:
            update_status(
                {
                    "state": "failure",
                    "run_id": RUN_ID,
                    "run_url": RUN_URL,
                    "error": str(error),
                    "diagnostic": diagnostic,
                },
                "ci: record failed SetWindowRgn v12b run",
            )
        except Exception as status_error:
            print(f"failed to publish failure status: {status_error}", file=sys.stderr)
        raise
