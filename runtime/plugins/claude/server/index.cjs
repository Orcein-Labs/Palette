// SPDX-License-Identifier: AGPL-3.0-only
'use strict';
// Forward stdio unchanged to an administrator-configured Palette launcher.
const fs = require('node:fs');
const { spawn } = require('node:child_process');
try {
  const config = JSON.parse(fs.readFileSync(process.env.PALETTE_LAUNCH_CONFIG, 'utf8'));
  if (typeof config.command !== 'string' || !config.command ||
      !Array.isArray(config.args) || config.args.some(x => typeof x !== 'string') ||
      (config.env && (typeof config.env !== 'object' || Array.isArray(config.env) ||
        Object.values(config.env).some(x => typeof x !== 'string')))) {
    throw new Error('launch config requires command, string args, and optional string env');
  }
  const child = spawn(config.command, config.args, {
    env: { ...process.env, ...config.env }, stdio: ['pipe', 'pipe', 'inherit'], shell: false
  });
  child.on('error', error => { console.error(`Palette launch failed: ${error.message}`); process.exitCode = 1; process.stdin.destroy(); });
  process.stdin.pipe(child.stdin);
  child.stdin.on('error', error => { if (error.code !== 'EPIPE') console.error(error.message); });
  child.stdout.pipe(process.stdout);
  child.on('exit', (code, signal) => { process.exitCode = code === null ? 1 : code; process.stdin.destroy(); });
  for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, () => child.kill(signal));
} catch (error) {
  console.error(`Palette configuration error: ${error.message}`);
  process.exitCode = 1;
}
