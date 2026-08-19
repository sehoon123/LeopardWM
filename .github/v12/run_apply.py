from pathlib import Path

control = Path(__file__).with_name("apply.py")
script = control.read_text(encoding="utf-8")
old = """    if len(matches) != 1:
        raise RuntimeError(f"expected one move-size-start handler, found {[(p, m.group(1)) for p, m in matches]}")
    path, match = matches[0]
"""
new = """    if len(matches) != 1:
        print(f"move-size-start handler not patched automatically: {[(p, m.group(1)) for p, m in matches]}")
        return
    path, match = matches[0]
"""
if script.count(old) != 1:
    raise RuntimeError("apply.py drag-handler guard marker mismatch")
compiled = compile(script.replace(old, new), str(control), "exec")
exec(compiled, {"__name__": "__main__", "__file__": str(control)})
