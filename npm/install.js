#!/usr/bin/env node
'use strict';

// Fetches the parlar release tarball for this machine, checks its sha256, and puts the native
// parlar and parlard in npm/bin/native/, which launch.js and the plugin launchers run.

const crypto = require('node:crypto');
const fs = require('node:fs');
const https = require('node:https');
const os = require('node:os');
const path = require('node:path');
const { execFileSync } = require('node:child_process');

const pkg = require('../package.json');

// the release builds there are, per platform and CPU
const TARGETS = {
  'linux-x64': 'x86_64-unknown-linux-gnu',
  'linux-arm64': 'aarch64-unknown-linux-gnu',
  'win32-x64': 'x86_64-pc-windows-msvc',
  'darwin-arm64': 'aarch64-apple-darwin',
};
// on Windows libmoonshine is linked in, and ONNX Runtime and the Visual C++ runtime ship next to
// the binaries (a fresh Windows has no VC++ runtime)
const WINDOWS_RUNTIME = ['onnxruntime.dll', 'vcruntime140.dll', 'vcruntime140_1.dll', 'msvcp140.dll'];
// on macOS libmoonshine is linked in too, and ONNX Runtime ships next to the binaries
const binaries =
  process.platform === 'win32'
    ? ['parlar.exe', 'parlard.exe', 'parlar-overlay.exe', ...WINDOWS_RUNTIME]
    : process.platform === 'darwin'
      ? ['parlar', 'parlard', 'parlar-overlay', 'libonnxruntime.1.23.0.dylib']
      : ['parlar', 'parlard'];
const nativeDir = path.join(__dirname, 'bin', 'native');

function fail(message) {
  console.error(`[parlar] ${message}`);
  process.exit(1);
}

function install(fromDir) {
  fs.mkdirSync(nativeDir, { recursive: true });
  for (const name of binaries) {
    const src = path.join(fromDir, name);
    if (!fs.existsSync(src)) {
      throw new Error(`${name} is missing from ${fromDir}`);
    }
    // copy then rename, so a running parlard keeps the binary it mapped
    const tmp = path.join(nativeDir, `.${name}.${process.pid}`);
    fs.copyFileSync(src, tmp);
    fs.chmodSync(tmp, 0o755);
    fs.renameSync(tmp, path.join(nativeDir, name));
  }
}

function download(url, destination, redirects = 5) {
  return new Promise((resolve, reject) => {
    const request = https.get(url, { headers: { 'user-agent': `parlar-npm/${pkg.version}` } }, (response) => {
      if (response.statusCode >= 300 && response.statusCode < 400 && response.headers.location && redirects > 0) {
        response.resume();
        download(new URL(response.headers.location, url).toString(), destination, redirects - 1).then(resolve, reject);
        return;
      }
      if (response.statusCode !== 200) {
        response.resume();
        reject(new Error(`download failed with HTTP ${response.statusCode}: ${url}`));
        return;
      }
      const file = fs.createWriteStream(destination, { mode: 0o600 });
      response.pipe(file);
      file.on('finish', () => file.close(resolve));
      file.on('error', reject);
    });
    request.on('error', reject);
  });
}

function sha256File(filePath) {
  return crypto.createHash('sha256').update(fs.readFileSync(filePath)).digest('hex');
}

function nextSteps() {
  console.log(
    [
      '[parlar] installed. Next:',
      '  parlard fetch       download the speech models and libmoonshine (about 800 MB, once)',
      '  parlard service     run parlard as a systemd user service',
      '  then in Claude Code: /plugin marketplace add agent-sh/parlar, /plugin install parlar@parlar',
    ].join('\n')
  );
}

async function main() {
  if (process.env.PARLAR_NPM_SKIP_DOWNLOAD === '1') {
    console.log('[parlar] skipping binary download');
    return;
  }
  // CI and packagers: take the binaries from a local build instead of a release
  if (process.env.PARLAR_NPM_LOCAL_DIR) {
    install(process.env.PARLAR_NPM_LOCAL_DIR);
    console.log(`[parlar] installed local binaries from ${process.env.PARLAR_NPM_LOCAL_DIR}`);
    return;
  }
  const target = TARGETS[`${process.platform}-${process.arch}`];
  if (!target) {
    fail(`no parlar build for ${process.platform} ${process.arch}. Builds: Linux x64 and arm64, Windows x64, macOS arm64.`);
  }

  const tag = `v${pkg.version}`;
  const asset = `parlar-${tag}-${target}`;
  const base = process.env.PARLAR_NPM_DOWNLOAD_BASE || `https://github.com/agent-sh/parlar/releases/download/${tag}`;
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'parlar-npm-'));
  try {
    const tarball = path.join(tmp, `${asset}.tar.gz`);
    const shaFile = `${tarball}.sha256`;
    console.log(`[parlar] downloading ${asset}.tar.gz`);
    await download(`${base}/${asset}.tar.gz`, tarball);
    await download(`${base}/${asset}.tar.gz.sha256`, shaFile);
    const want = (fs.readFileSync(shaFile, 'utf8').match(/\b[a-fA-F0-9]{64}\b/) || [])[0];
    // throw, not fail(): process.exit would skip the cleanup in finally
    if (!want) {
      throw new Error('the .sha256 file has no digest');
    }
    const got = sha256File(tarball);
    if (got !== want.toLowerCase()) {
      throw new Error(`sha256 mismatch for ${asset}.tar.gz: expected ${want}, got ${got}`);
    }
    execFileSync('tar', ['xzf', tarball, '-C', tmp], { stdio: 'inherit' });
    install(path.join(tmp, asset));
    nextSteps();
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
}

main().catch((error) => fail(error.message));
