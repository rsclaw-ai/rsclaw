#!/usr/bin/env python3
"""Generate the skill/plugin auto-install allowlist + packages from a curated set.

Walks a skills dir and a plugins dir and emits the hub layout:

    <out>/allowlist/meta.json       (version + per-list sha256 integrity)
    <out>/allowlist/skills.json     ({"skills":[{slug, url, version, sha256, ...}]})
    <out>/allowlist/plugins.json    ({"plugins":[...]})
    <out>/skills/<slug>.zip         (the audited skill package)
    <out>/plugins/<slug>.zip        (the audited plugin package)

The agent installs ONLY from the allowlist `url` (a direct package URL), never
through a public registry. So each entry carries a download URL; the hub serves
the matching `<out>/skills|plugins/<slug>.zip`.

Hashes must match what the gateway verifies:
  - skill entry `sha256`  = sha256 of the skill's SKILL.md raw bytes
                            (src/skill/allowlist.rs::verify_skill_content)
  - plugin entry `sha256` = sha256 of the plugin's manifest raw bytes
  - meta per-list sha256  = sha256 of the EXACT *.json bytes written
                            (src/skill/allowlist.rs::verify_against_meta)

Package zips put the skill/plugin dir CONTENTS at the zip root (SKILL.md /
scripts/ at top level), because the gateway extracts the zip INTO
`~/.rsclaw/skills/<slug>/` (clawhub::install_from_url -> extract_zip).

Usage:
    scripts/gen-allowlist.py [--skills-dir ~/.rsclaw/skills]
        [--plugins-dir ~/.rsclaw/plugins] [--out dist]
        [--url-base https://api.rsclaw.ai/v1/hub] [--publisher "同花顺"]
"""
import argparse
import datetime
import hashlib
import json
import os
import sys
import zipfile

MANIFEST_CANDIDATES = ("plugin.json5", "openclaw.plugin.json", "plugin.json")


def sha256_file(path: str) -> str:
    with open(path, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()


def sha256_str(s: str) -> str:
    return hashlib.sha256(s.encode("utf-8")).hexdigest()


def parse_frontmatter(skill_md_path: str) -> dict:
    out = {}
    try:
        with open(skill_md_path, "r", encoding="utf-8") as f:
            text = f.read()
    except OSError:
        return out
    if not text.startswith("---"):
        return out
    end = text.find("\n---", 3)
    block = text[3:end] if end != -1 else ""
    for line in block.splitlines():
        if ":" in line and not line.lstrip().startswith("#"):
            k, _, v = line.partition(":")
            out[k.strip()] = v.strip().strip('"').strip("'")
    return out


def zip_dir_contents(src_dir: str, out_zip: str) -> None:
    """Zip the CONTENTS of src_dir (files at the zip root), deterministically."""
    os.makedirs(os.path.dirname(out_zip), exist_ok=True)
    files = []
    for root, _, names in os.walk(src_dir):
        for n in names:
            full = os.path.join(root, n)
            files.append((os.path.relpath(full, src_dir), full))
    files.sort()
    with zipfile.ZipFile(out_zip, "w", zipfile.ZIP_DEFLATED) as z:
        for arc, full in files:
            zi = zipfile.ZipInfo(arc, date_time=(1980, 1, 1, 0, 0, 0))
            zi.compress_type = zipfile.ZIP_DEFLATED
            with open(full, "rb") as fh:
                z.writestr(zi, fh.read())


def collect_skills(skills_dir, out, url_base, publisher, audited_at) -> list:
    entries = []
    if not os.path.isdir(skills_dir):
        return entries
    for dirname in sorted(os.listdir(skills_dir)):
        d = os.path.join(skills_dir, dirname)
        md = os.path.join(d, "SKILL.md")
        if not os.path.isfile(md):
            continue
        fm = parse_frontmatter(md)
        slug = fm.get("name", dirname)
        zip_dir_contents(d, os.path.join(out, "skills", f"{slug}.zip"))
        entries.append({
            "slug": slug,
            "url": f"{url_base}/skills/{slug}.zip",
            "version": fm.get("version", ""),
            "sha256": sha256_file(md),         # gateway pins SKILL.md
            "publisher": publisher,
            "audited_at": audited_at,
        })
    return entries


def collect_plugins(plugins_dir, out, url_base, publisher, audited_at) -> list:
    entries = []
    if not os.path.isdir(plugins_dir):
        return entries
    for slug in sorted(os.listdir(plugins_dir)):
        d = os.path.join(plugins_dir, slug)
        if not os.path.isdir(d):
            continue
        manifest = next((os.path.join(d, m) for m in MANIFEST_CANDIDATES
                         if os.path.isfile(os.path.join(d, m))), None)
        if manifest is None:
            print(f"  skip plugin {slug}: no manifest", file=sys.stderr)
            continue
        zip_dir_contents(d, os.path.join(out, "plugins", f"{slug}.zip"))
        entries.append({
            "slug": slug,
            "url": f"{url_base}/plugins/{slug}.zip",
            "version": "",
            "sha256": sha256_file(manifest),   # plugin pin = manifest (interim)
            "publisher": publisher,
            "audited_at": audited_at,
        })
    return entries


def dump(obj) -> str:
    return json.dumps(obj, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def main() -> int:
    home = os.path.expanduser("~")
    ap = argparse.ArgumentParser(description="Generate the rsclaw skill/plugin allowlist + packages.")
    ap.add_argument("--skills-dir", default=os.path.join(home, ".rsclaw", "skills"))
    ap.add_argument("--plugins-dir", default=os.path.join(home, ".rsclaw", "plugins"))
    ap.add_argument("--out", default="dist")
    ap.add_argument("--url-base", default="https://api.rsclaw.ai/v1/hub")
    ap.add_argument("--publisher", default="")
    args = ap.parse_args()

    base = args.url_base.rstrip("/")
    audited_at = datetime.date.today().isoformat()
    skills = collect_skills(args.skills_dir, args.out, base, args.publisher, audited_at)
    plugins = collect_plugins(args.plugins_dir, args.out, base, args.publisher, audited_at)

    skills_json = dump({"skills": skills})
    plugins_json = dump({"plugins": plugins})
    meta_json = dump({
        "schema": 1,
        "version": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d.%H%M%S"),
        "updated_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "sha256": {"skills": sha256_str(skills_json), "plugins": sha256_str(plugins_json)},
    })

    al_dir = os.path.join(args.out, "allowlist")
    os.makedirs(al_dir, exist_ok=True)
    for name, content in (("skills.json", skills_json),
                          ("plugins.json", plugins_json),
                          ("meta.json", meta_json)):
        with open(os.path.join(al_dir, name), "w", encoding="utf-8") as f:
            f.write(content)

    print(f"wrote {args.out}/  ({len(skills)} skills, {len(plugins)} plugins)")
    for e in skills:
        print(f"  skill  {e['slug']:32} {e['sha256'][:12]}  {e['url']}")
    for e in plugins:
        print(f"  plugin {e['slug']:32} {e['sha256'][:12]}  {e['url']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
