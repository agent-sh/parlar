'use strict';

// Runs the native binary that npm/install.js put in native/ next to this file. A Node launcher
// works from cmd, PowerShell and any shell alike; the parlar plugin launchers skip it and run the
// native binary directly, so hooks do not pay for starting Node.

const fs = require('node:fs');
const path = require('node:path');
const { spawn } = require('node:child_process');

module.exports = function launch(name) {
  const exe = process.platform === 'win32' ? `${name}.exe` : name;
  const bin = path.join(__dirname, 'native', exe);
  if (!fs.existsSync(bin)) {
    console.error(`${name}: native binary missing at ${bin}; run npm rebuild -g @agent-sh/parlar`);
    process.exit(127);
  }
  const child = spawn(bin, process.argv.slice(2), { stdio: 'inherit' });
  for (const signal of ['SIGINT', 'SIGTERM', 'SIGHUP']) {
    process.on(signal, () => child.kill(signal));
  }
  child.on('error', (e) => {
    console.error(`${name}: ${e.message}`);
    process.exit(127);
  });
  child.on('exit', (code, signal) => process.exit(signal ? 1 : (code ?? 1)));
};
