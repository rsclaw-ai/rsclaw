#!/usr/bin/env node
// Cross-platform port of fetch-bundled-model.sh — Tauri's beforeBuildCommand
// runs in cmd.exe on Windows, where `bash` is not guaranteed (no Git Bash on
// fresh GitHub windows-latest runners). Pure Node so the Windows desktop
// release workflow stops failing with exit code 1 on the bash call.
//
// Idempotent: skips when the bundled file is already present and non-empty.

import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { execSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const REPO_ROOT = path.resolve(__dirname, '..');
const TARGET_DIR = path.join(REPO_ROOT, 'ui', 'src-tauri', 'resources', 'bge-small-zh');
const TARGET_FILE = path.join(TARGET_DIR, 'model.safetensors');
const URL = process.env.BGE_MODEL_URL || 'https://gitfast.org/tools/models/bge-small-zh-v1.5.zip';

const log = (msg) => console.log(`[fetch-bundled-model] ${msg}`);
const sizeOf = (p) => { try { return fs.statSync(p).size; } catch { return 0; } };

if (sizeOf(TARGET_FILE) > 0) {
  log(`${TARGET_FILE} already present, skipping`);
  process.exit(0);
}

fs.mkdirSync(TARGET_DIR, { recursive: true });

// Local fallback: copy from user's existing model dir if present.
const LOCAL_SRC = path.join(os.homedir(), '.rsclaw', 'models', 'bge-small-zh');
if (sizeOf(path.join(LOCAL_SRC, 'model.safetensors')) > 0) {
  log(`copying from ${LOCAL_SRC}`);
  fs.copyFileSync(path.join(LOCAL_SRC, 'model.safetensors'), TARGET_FILE);
  for (const f of ['config.json', 'tokenizer.json']) {
    const dest = path.join(TARGET_DIR, f);
    if (sizeOf(dest) === 0) fs.copyFileSync(path.join(LOCAL_SRC, f), dest);
  }
  log('done');
  process.exit(0);
}

log(`downloading ${URL}`);
const workDir = fs.mkdtempSync(path.join(os.tmpdir(), 'bge-'));
const zipPath = path.join(workDir, 'bge-model.zip');
const extractDir = path.join(workDir, 'extract');
fs.mkdirSync(extractDir, { recursive: true });

const cleanup = () => { try { fs.rmSync(workDir, { recursive: true, force: true }); } catch {} };
process.on('exit', cleanup);

try {
  // Node 18+ has fetch built in. GitHub runners ship Node 20.
  const resp = await fetch(URL);
  if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
  const buf = Buffer.from(await resp.arrayBuffer());
  fs.writeFileSync(zipPath, buf);

  // bsdtar (the `tar` shipped with Windows 10+, macOS, and most Linux distros)
  // handles zip via libarchive. Avoids depending on `unzip` on Windows.
  execSync(`tar -xf "${zipPath}" -C "${extractDir}"`, { stdio: 'inherit' });

  const findFile = (dir, name) => {
    for (const ent of fs.readdirSync(dir, { withFileTypes: true })) {
      const p = path.join(dir, ent.name);
      if (ent.isDirectory()) {
        const found = findFile(p, name);
        if (found) return found;
      } else if (ent.name === name) {
        return p;
      }
    }
    return null;
  };

  const weights = findFile(extractDir, 'model.safetensors');
  if (!weights) throw new Error('zip did not contain model.safetensors');
  const srcDir = path.dirname(weights);
  fs.copyFileSync(weights, TARGET_FILE);
  for (const f of ['config.json', 'tokenizer.json']) {
    const dest = path.join(TARGET_DIR, f);
    if (sizeOf(dest) === 0) fs.copyFileSync(path.join(srcDir, f), dest);
  }
  log(`installed -> ${TARGET_FILE}`);
} catch (e) {
  console.error(`[fetch-bundled-model] ${e.message}`);
  process.exit(1);
}
