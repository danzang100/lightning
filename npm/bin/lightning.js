#!/usr/bin/env node
// Launcher: find or fetch the pinned, checksum-verified lightning binary, then run it.
"use strict";
const { spawnSync, execFileSync } = require("node:child_process");
const crypto = require("node:crypto");
const fs = require("node:fs");
const https = require("node:https");
const os = require("node:os");
const path = require("node:path");

const pkg = require("../package.json");
const checksums = require("../checksums.json");
const REPO = "danzang100/lightning";

function target() {
  const arch = { x64: "x86_64", arm64: "aarch64" }[process.arch];
  const sys = { linux: "unknown-linux-gnu", darwin: "apple-darwin", win32: "pc-windows-msvc" }[process.platform];
  if (!arch || !sys || (process.platform === "win32" && arch !== "x86_64")) {
    fail(`no prebuilt binary for ${process.platform}/${process.arch}; build from source: cargo install lightning-ledger`);
  }
  return `${arch}-${sys}`;
}

function fail(msg) {
  process.stderr.write(`lightning-ledger: ${msg}\n`);
  process.exit(1);
}

function cacheDir() {
  const base = process.env.XDG_CACHE_HOME || (process.platform === "win32" ? process.env.LOCALAPPDATA : null) || path.join(os.homedir(), ".cache");
  return path.join(base, "lightning-ledger", pkg.version);
}

function download(url, dest, redirects = 5) {
  return new Promise((resolve, reject) => {
    https
      .get(url, { headers: { "User-Agent": `lightning-ledger/${pkg.version}` } }, (res) => {
        if ([301, 302, 303, 307, 308].includes(res.statusCode) && res.headers.location && redirects > 0) {
          res.resume();
          return resolve(download(res.headers.location, dest, redirects - 1));
        }
        if (res.statusCode !== 200) {
          res.resume();
          return reject(new Error(`GET ${url}: HTTP ${res.statusCode}`));
        }
        const out = fs.createWriteStream(dest);
        res.pipe(out);
        out.on("finish", () => out.close(resolve));
        out.on("error", reject);
      })
      .on("error", reject);
  });
}

function sha256(file) {
  return crypto.createHash("sha256").update(fs.readFileSync(file)).digest("hex");
}

async function ensureBinary() {
  if (process.env.LIGHTNING_BINARY) return process.env.LIGHTNING_BINARY;
  const t = target();
  const exe = process.platform === "win32" ? "lightning.exe" : "lightning";
  const dir = cacheDir();
  const bin = path.join(dir, exe);
  if (fs.existsSync(bin)) return bin;

  const asset = `lightning-${t}${process.platform === "win32" ? ".zip" : ".tar.gz"}`;
  const expected = checksums[asset];
  if (!expected) fail(`this package has no checksum for ${asset}; refusing to download it`);
  const url = `https://github.com/${REPO}/releases/download/v${pkg.version}/${asset}`;
  fs.mkdirSync(dir, { recursive: true });
  const archive = path.join(dir, asset);
  process.stderr.write(`lightning-ledger: downloading ${url}\n`);
  await download(url, archive);
  const got = sha256(archive);
  if (got !== expected) {
    fs.rmSync(archive, { force: true });
    fail(`checksum mismatch for ${asset}: expected ${expected}, got ${got}; nothing was run`);
  }
  process.stderr.write(`lightning-ledger: verified sha256 ${got}\n`);
  // tar ships with Windows 10+, macOS and Linux and reads both .tar.gz and .zip.
  execFileSync("tar", ["-xf", archive, "-C", dir], { stdio: "inherit" });
  fs.rmSync(archive, { force: true });
  if (!fs.existsSync(bin)) fail(`archive ${asset} did not contain ${exe}`);
  if (process.platform !== "win32") fs.chmodSync(bin, 0o755);
  return bin;
}

ensureBinary()
  .then((bin) => {
    const r = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });
    if (r.error) fail(r.error.message);
    process.exit(r.status === null ? 1 : r.status);
  })
  .catch((e) => fail(e.message));
