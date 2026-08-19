from pathlib import Path

path = Path(__file__).with_name("pipeline.py")
script = path.read_text(encoding="utf-8")
start_marker = '    audit = ROOT / "setwindowrgn-v12-audit.md"\n'
end_marker = '    if run(["gh", "release", "view", TAG, "--repo", REPO], check=False).returncode == 0:\n'
start = script.find(start_marker)
end = script.find(end_marker, start)
if start < 0 or end < 0:
    raise RuntimeError("pipeline release-text section markers missing")
replacement = '''    audit = ROOT / "setwindowrgn-v12-audit.md"
    audit.write_text(
        "# SetWindowRgn v12 safety audit\\n\\n"
        "- Transactional active/pending HWND markers cover crash windows.\\n"
        "- Existing and replacement application regions are never cleared.\\n"
        "- Known-hung HWNDs use the whole-window fallback.\\n"
        "- Region frames use post-position actual outer HWND geometry.\\n"
        "- Clipped animation HWNDs use synchronous adaptive dispatch.\\n"
        "- Preferred containment is verified; last-resort parking is fail-closed.\\n"
        "- Region specifications participate in the daemon layout fast-path key.\\n"
        "- Lifecycle recovery covers clip removal, empty layouts, drag start when discoverable, shutdown/revert, emergency uncloak, and HWND destruction.\\n"
        "- Debug/release tests, Clippy, real HWND ownership tests, Settings renders, MSI install, and published-asset identity gates passed.\\n",
        encoding="utf-8",
    )
    notes = ROOT / "release-notes-v12.md"
    notes.write_text(
        "Real SetWindowRgn monitor clipping for the personal LeopardWM line.\\n\\n"
        "- Preserves partial tiled previews within the owning monitor.\\n"
        "- Clips only pixels crossing a physical monitor boundary.\\n"
        "- Keeps `monitor_overflow = \\"hide\\"` as the conservative fallback.\\n"
        "- Preserves application-defined/replaced regions.\\n"
        "- Uses transactional crash-recovery markers and verified post-position geometry.\\n"
        "- Adds the Settings GUI selector with load/save/default validation.\\n",
        encoding="utf-8",
    )

'''
compiled = compile(script[:start] + replacement + script[end:], str(path), "exec")
exec(compiled, {"__name__": "__main__", "__file__": str(path)})
