#!/usr/bin/env python3
"""Refreshes the release standings bundled from TRaSH Guides (https://trash-guides.info).

    python3 -I scripts/trash/update.py [COMMIT]

Reads Radarr's and Sonarr's custom formats at COMMIT of TRaSH-Guides/Guides (default: the
newest on master) and writes crates/search/src/trash/formats.json and its LICENSE. Only the
release group and release title conditions are kept: the search ranks names, and knows
nothing of a release's source or resolution the way Radarr does after it grabs one.
"""

import json
import sys
import urllib.request
from pathlib import Path

REPO = "TRaSH-Guides/Guides"
OUT = Path(__file__).resolve().parents[2] / "crates/search/src/trash"

# Formats that mark a release or its group as one to avoid, whatever else it is.
PENALTIES = [
    "lq",
    "lq-release-title",
    "retags",
    "obfuscated",
    "upscaled",
    "bad-dual-groups",
    "br-disk",
    "3d",
    "anime-lq-groups",
]
# Formats that rank groups by the quality of their releases, tier 01 first.
TIERS = [f"{kind}-tier-0{n}" for kind in ("web", "hd-bluray", "uhd-bluray", "remux") for n in (1, 2, 3)]

ON = {"ReleaseGroupSpecification": "group", "ReleaseTitleSpecification": "title"}


def get(url):
    request = urllib.request.Request(url, headers={"User-Agent": "newsreel-trash-update"})
    with urllib.request.urlopen(request, timeout=30) as response:
        return response.read()


def newest():
    commit = json.loads(get(f"https://api.github.com/repos/{REPO}/commits/master"))
    return commit["sha"]


def custom_format(commit, app, name):
    url = f"https://raw.githubusercontent.com/{REPO}/{commit}/docs/json/{app}/cf/{name}.json"
    try:
        return json.loads(get(url))
    except urllib.error.HTTPError as e:
        if e.code == 404:
            return None
        raise


def entry(app, name, kind, tier, cf):
    specs = [
        {
            "on": ON[s["implementation"]],
            "required": s["required"],
            "negate": s["negate"],
            "name": s["name"],
            "pattern": s["fields"]["value"],
        }
        for s in cf["specifications"]
        if s["implementation"] in ON
    ]
    return {"app": app, "format": name, "title": cf["name"], "kind": kind, "tier": tier, "specs": specs}


def main():
    commit = sys.argv[1] if len(sys.argv) > 1 else newest()
    formats = []
    for app in ("radarr", "sonarr"):
        for name in PENALTIES:
            cf = custom_format(commit, app, name)
            if cf:
                formats.append(entry(app, name, "penalty", None, cf))
        for name in TIERS:
            cf = custom_format(commit, app, name)
            if cf:
                formats.append(entry(app, name, "tier", int(name[-1]), cf))
    OUT.mkdir(parents=True, exist_ok=True)
    document = {"source": f"https://github.com/{REPO}", "commit": commit, "formats": formats}
    (OUT / "formats.json").write_text(json.dumps(document, indent=1, ensure_ascii=False) + "\n")
    (OUT / "LICENSE").write_bytes(get(f"https://raw.githubusercontent.com/{REPO}/{commit}/LICENSE"))
    specs = sum(len(f["specs"]) for f in formats)
    print(f"wrote {len(formats)} formats with {specs} conditions from {REPO}@{commit[:12]}")


if __name__ == "__main__":
    main()
