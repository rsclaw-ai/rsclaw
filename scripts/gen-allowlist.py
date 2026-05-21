#!/usr/bin/env python3
"""Generate the skill/plugin auto-install allowlist from a curated local set.

Walks a skills dir and a plugins dir, and emits the hub layout:

    <out>/allowlist/meta.json      (version + per-list sha256 integrity)
    <out>/allowlist/skills.json    ({"skills":[{slug, version, sha256, ...}]})
    <out>/allowlist/plugins.json   ({"plugins":[...]})

The per-entry `sha256` MUST match what the gateway verifies on install:
  - skill : sha256 of the skill's SKILL.md raw bytes
            (src/skill/allowlist.rs::verify_skill_content)
  - plugin: sha256 of the plugin's manifest (plugin.json5) raw bytes
The per-list sha256 in meta.json is over the EXACT bytes of the written
*.json file (src/skill/allowlist.rs::verify_against_meta), so we hash the
serialized string we write — no extra trailing newline.

Usage:
    scripts/gen-allowlist.py \
        [--skills-dir ~/.rsclaw/skills] \
        [--plugins-dir ~/.rsclaw/plugins] \
        [--out dist] \
        [--publisher "同花顺"]

These files are only the *content*; serving them at
https://api.rsclaw.ai/v1/hub/allowlist/ and (optionally) signing meta.json is
the hub side.
"""
import argparse
import datetime
import hashlib
import json
import os
import sys

MANIFEST_CANDIDATES = ("plugin.json5", "openclaw.plugin.json", "plugin.json")


def sha256_file(path: str) -> str:
    with open(path, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()


def sha256_str(s: str) -> str:
    return hashlib.sha256(s.encode("utf-8")).hexdigest()


def parse_frontmatter(skill_md_path: str) -> dict:
    """Pull name/version/description out of the SKILL.md YAML frontmatter
    without a YAML dependency (simple key: value scan between the --- fences)."""
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


def collect_skills(skills_dir: str, publisher: str, audited_at: str) -> list:
    entries = []
    if not os.path.isdir(skills_dir):
        return entries
    for slug in sorted(os.listdir(skills_dir)):
        d = os.path.join(skills_dir, slug)
        md = os.path.join(d, "SKILL.md")
        if not os.path.isfile(md):
            continue
        fm = parse_frontmatter(md)
        entries.append({
            "slug": fm.get("name", slug),
            "version": fm.get("version", ""),
            "sha256": sha256_file(md),
            "publisher": publisher,
            "audited_at": audited_at,
        })
    return entries


def collect_plugins(plugins_dir: str, publisher: str, audited_at: str) -> list:
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
        entries.append({
            "slug": slug,
            "version": "",
            "sha256": sha256_file(manifest),
            "publisher": publisher,
            "audited_at": audited_at,
        })
    return entries


def dump(obj) -> str:
    # Deterministic + the exact bytes meta.json hashes and the hub serves.
    return json.dumps(obj, ensure_ascii=False, sort_keys=True,
                      separators=(",", ":"))


def main() -> int:
    home = os.path.expanduser("~")
    ap = argparse.ArgumentParser(description="Generate the rsclaw skill/plugin allowlist.")
    ap.add_argument("--skills-dir", default=os.path.join(home, ".rsclaw", "skills"))
    ap.add_argument("--plugins-dir", default=os.path.join(home, ".rsclaw", "plugins"))
    ap.add_argument("--out", default="dist")
    ap.add_argument("--publisher", default="")
    args = ap.parse_args()

    audited_at = datetime.date.today().isoformat()
    skills = collect_skills(args.skills_dir, args.publisher, audited_at)
    plugins = collect_plugins(args.plugins_dir, args.publisher, audited_at)

    skills_json = dump({"skills": skills})
    plugins_json = dump({"plugins": plugins})
    meta_json = dump({
        "schema": 1,
        "version": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d.%H%M%S"),
        "updated_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "sha256": {"skills": sha256_str(skills_json), "plugins": sha256_str(plugins_json)},
    })

    out_dir = os.path.join(args.out, "allowlist")
    os.makedirs(out_dir, exist_ok=True)
    for name, content in (("skills.json", skills_json),
                          ("plugins.json", plugins_json),
                          ("meta.json", meta_json)):
        with open(os.path.join(out_dir, name), "w", encoding="utf-8") as f:
            f.write(content)

    print(f"wrote {out_dir}/  ({len(skills)} skills, {len(plugins)} plugins)")
    for e in skills:
        print(f"  skill  {e['slug']:32} {e['sha256'][:12]}")
    for e in plugins:
        print(f"  plugin {e['slug']:32} {e['sha256'][:12]}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
