"""Keeps a short "The Octo apps" block at the end of the Latest release's notes.

The Android and desktop apps publish their releases here too, but never as
Latest (the server keeps that badge), so on the repository's front page and
at the top of the Releases page only the server shows. The block links the
newest app releases from there. It is rewritten in the Latest release on
every run and taken out of any other release, so it is only ever in one
place and never goes stale.

Run by .github/workflows/release-links.yml whenever a release is published;
by hand: `GH_REPO=winters27/octo python .github/scripts/app_links.py [--dry-run]`
(needs `gh`, signed in with write access for a real run).
"""

import json
import os
import re
import subprocess
import sys

REPO = os.environ.get("GH_REPO", "winters27/octo")
START = "<!-- octo-apps: kept up to date by .github/workflows/release-links.yml -->"
END = "<!-- /octo-apps -->"
BLOCK = re.compile(r"\s*" + re.escape(START) + r".*?" + re.escape(END) + r"\s*", re.S)
FDROID = "https://winters27.github.io/octo/fdroid/"
APPS = [("android-v", "Octo for Android"), ("desktop-v", "Octo for Windows and Linux")]


def gh(*args, body=None):
    out = subprocess.run(
        ["gh", "api", *args] + (["--input", "-"] if body is not None else []),
        input=json.dumps(body) if body is not None else None,
        capture_output=True, text=True, encoding="utf-8", check=True,
    ).stdout
    return json.loads(out) if out.strip() else None


def without_block(text):
    return BLOCK.sub("\n", text or "").rstrip()


def block(releases):
    links = []
    for prefix, fallback in APPS:
        stable = [r for r in releases if r["tag_name"].startswith(prefix) and not r["draft"] and not r["prerelease"]]
        if stable:
            newest = max(stable, key=lambda r: r["published_at"] or "")
            links.append(f"[{newest['name'] or fallback}]({newest['html_url']})")
    if not links:
        return None
    links.append(f"[Octo's F-Droid repository]({FDROID})")
    return f"{START}\n\n---\n\n**The Octo apps:** " + " · ".join(links) + f"\n\n{END}"


def main():
    dry = "--dry-run" in sys.argv
    releases = gh(f"repos/{REPO}/releases?per_page=100")
    latest = gh(f"repos/{REPO}/releases/latest")
    apps = block(releases)
    for release in releases:
        is_latest = release["id"] == latest["id"]
        old = release["body"] or ""
        if not is_latest and START not in old:
            continue
        new = without_block(old)
        if is_latest and apps:
            new = f"{new}\n\n{apps}" if new else apps
        if new == old:
            print(f"{release['tag_name']}: already right")
            continue
        print(f"{release['tag_name']}: {'block written' if is_latest else 'block taken out'}")
        if dry:
            print(new[-600:])
        else:
            gh("--method", "PATCH", f"repos/{REPO}/releases/{release['id']}", body={"body": new})


if __name__ == "__main__":
    main()
