#!/usr/bin/env python3
"""Fail on broken relative links in Markdown files (docs must not rot)."""
import os, re, subprocess, sys

root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
try:
    files = subprocess.run(["git", "ls-files", "*.md"], cwd=root, capture_output=True, text=True, check=True).stdout.split()
except Exception:
    files = [os.path.relpath(os.path.join(d, f), root) for d, _, fs in os.walk(root) if "/target" not in d and "/.git" not in d for f in fs if f.endswith(".md")]
bad = []
for f in files:
    text = open(os.path.join(root, f), encoding="utf-8").read()
    text = re.sub(r"```.*?```", "", text, flags=re.S)
    for target in re.findall(r"\]\(([^)\s]+)\)", text):
        if re.match(r"^[a-z]+:", target) or target.startswith("#"):
            continue
        path = target.split("#")[0]
        if path and not os.path.exists(os.path.normpath(os.path.join(root, os.path.dirname(f), path))):
            bad.append(f"{f}: {target}")
if bad:
    print("Broken links:\n" + "\n".join(bad))
    sys.exit(1)
print(f"ok ({len(files)} files)")
