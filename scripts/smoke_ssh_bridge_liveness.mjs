#!/usr/bin/env bun
// Linux-only, rootless network-namespace smoke. No host firewall or SSH configuration changes.
import { spawn, spawnSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readlinkSync, rmSync, writeFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { userInfo } from 'node:os';
import { fileURLToPath } from 'node:url';

const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
function run(program, args, env = process.env) {
  const result = spawnSync(program, args, { encoding: 'utf8', env });
  if (result.status !== 0) throw new Error(`${program}: ${result.stderr || result.error || result.status}`);
  return result.stdout;
}
async function until(predicate, milliseconds, label) {
  const deadline = Date.now() + milliseconds;
  while (!predicate()) {
    if (Date.now() >= deadline) throw new Error(`timed out: ${label}`);
    await delay(50);
  }
}
function string(value) {
  const bytes = Buffer.from(value);
  const length = bytes.length < 251 ? Buffer.from([bytes.length]) : Buffer.from([251, bytes.length & 255, bytes.length >> 8]);
  return Buffer.concat([length, bytes]);
}
function control(kind, data) {
  // Generation-1 EndpointControl tag and length framing, also exercised in tests/support/mod.rs.
  const payload = Buffer.concat([Buffer.from([20]), string(kind), string(data)]);
  const size = Buffer.alloc(4);
  size.writeUInt32LE(payload.length);
  return Buffer.concat([size, payload]);
}
const quote = (text) => `'${text.replaceAll("'", "'\\''")}'`;
function cleanEnvironment(root) {
  const env = Object.fromEntries(Object.entries(process.env).filter(([key]) => !key.startsWith('HERDR_')));
  Object.assign(env, {
    XDG_CONFIG_HOME: `${root}/config`, XDG_STATE_HOME: `${root}/state`, XDG_RUNTIME_DIR: `${root}/run`,
    HERDR_CONFIG_PATH: `${root}/config.toml`, HERDR_SOCKET_PATH: `${root}/api.sock`, HERDR_CLIENT_SOCKET_PATH: `${root}/client.sock`,
    HERDR_DISABLE_SOUND: '1',
  });
  for (const path of [env.XDG_CONFIG_HOME, env.XDG_STATE_HOME, env.XDG_RUNTIME_DIR]) mkdirSync(path, { recursive: true });
  writeFileSync(env.HERDR_CONFIG_PATH, '[experimental]\nallow_nested = true\n');
  return env;
}

async function smoke(before, after) {
  const root = mkdtempSync('/var/tmp/herdr-ssh-liveness-');
  const children = [];
  const bridges = new Set();
  const launch = (program, args, options = {}) => {
    const child = spawn(program, args, { stdio: ['ignore', 'inherit', 'inherit'], ...options });
    children.push(child);
    return child;
  };
  let success = false;
  try {
    run('ip', ['link', 'set', 'lo', 'up']);
    run('ip', ['addr', 'add', '10.77.0.2/32', 'dev', 'lo']);
    run('nft', ['add', 'table', 'inet', 'herdr_smoke']);
    run('nft', ['add', 'chain', 'inet', 'herdr_smoke', 'output', '{ type filter hook output priority 0; policy accept; }']);
    for (const key of ['host', 'client']) run('ssh-keygen', ['-q', '-t', 'ed25519', '-N', '', '-f', `${root}/${key}`]);
    writeFileSync(`${root}/sshd_config`, `Port 22222\nListenAddress 10.77.0.2\nHostKey ${root}/host\nAuthorizedKeysFile ${root}/client.pub\nPidFile ${root}/sshd.pid\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nClientAliveInterval 0\nTCPKeepAlive no\nLogLevel ERROR\n`);
    const sshd = launch(Bun.which('sshd'), ['-D', '-e', '-f', `${root}/sshd_config`]);
    await delay(400);
    if (sshd.exitCode !== null) throw new Error('disposable sshd failed');
    const sockets = () => run('ss', ['-Hnt', 'sport', '=', '22222']).split('\n').filter((line) => line.startsWith('ESTAB'));
    const alive = (pid) => { try { process.kill(pid, 0); return true; } catch { return false; } };

    for (const [label, binary, expectedCleanup] of [['before', before, false], ['after', after, true]]) {
      const phase = `${root}/${label}`;
      mkdirSync(phase);
      const env = cleanEnvironment(phase);
      const server = launch(binary, ['--session', 'ssh-smoke', 'server'], { env });
      await until(() => {
        try { return JSON.parse(run(binary, ['--session', 'ssh-smoke', 'status', 'server', '--json'], env)).running; }
        catch { return false; }
      }, 10000, 'isolated Herdr server');
      const connections = [];
      for (let index = 0; index < 3; index++) {
        const remoteEnv = Object.entries(env).filter(([key]) => key.startsWith('HERDR_') || key.startsWith('XDG_')).map(([key, value]) => `${key}=${quote(value)}`).join(' ');
        const command = `printf 'BRIDGE %s %s\\n' "$$" "$PPID"; exec env ${remoteEnv} ${quote(binary)} --session ssh-smoke remote-client-bridge --idle-timeout-v1`;
        const ssh = launch('ssh', ['-F', '/dev/null', '-o', 'StrictHostKeyChecking=no', '-o', 'UserKnownHostsFile=/dev/null', '-o', 'BatchMode=yes', '-o', 'IdentitiesOnly=yes', '-o', 'IdentityAgent=none', '-i', `${root}/client`, '-p', '22222', `${userInfo().username}@10.77.0.2`, command], { stdio: ['pipe', 'pipe', 'inherit'] });
        let output = Buffer.alloc(0);
        let welcome = false;
        let ids;
        ssh.stdout.on('data', (chunk) => {
          output = Buffer.concat([output, chunk]);
          if (!ids) {
            const end = output.indexOf(10);
            if (end < 0) return;
            const match = output.subarray(0, end).toString().match(/^BRIDGE (\d+) (\d+)$/);
            if (!match) throw new Error('invalid bridge marker');
            ids = match.slice(1).map(Number);
            bridges.add(ids[0]);
            output = output.subarray(end + 1);
          }
          while (output.length >= 4 && output.length >= 4 + output.readUInt32LE(0)) {
            const size = output.readUInt32LE(0);
            const frame = output.subarray(4, 4 + size);
            if (frame.includes(Buffer.from('endpoint.welcome.v1'))) welcome = true;
            output = output.subarray(4 + size);
          }
        });
        await until(() => ids, 5000, 'bridge marker');
        ssh.stdin.write(control('endpoint.hello.v1', JSON.stringify({ generation: 1, cell_width_px: 8, cell_height_px: 16, surface_size: { cols: 80, rows: 24 }, pixel_mouse: false, direct_graphics: false, endpoint_keybindings: false, mouse_capture: false, surface_active: false, snapshot_codecs: ['shell.snapshot.v1'], surface_codecs: ['shell.surface.v1'], input_codecs: ['shell.input.semantic.v1'], blob_codecs: ['shell.blob.v1'] })));
        await until(() => welcome, 5000, 'real endpoint handshake');
        connections.push({ ssh, ids });
      }
      await delay(1000);
      console.log(`${label}: three real Herdr handshakes; suppressing disconnect packets`);
      run('nft', ['add', 'rule', 'inet', 'herdr_smoke', 'output', 'tcp', 'dport', '22222', 'drop']);
      for (const { ssh } of connections) ssh.kill('SIGKILL');
      await delay(200);
      run('ss', ['-K', 'dst', '10.77.0.2', 'dport', '=', '22222']);
      if (run('ss', ['-Hnt', 'dst', '10.77.0.2', 'dport', '=', '22222']).trim()) throw new Error('desktop sockets remain');
      if (sockets().length !== 3) throw new Error('failed to reproduce three abandoned remote SSH transports');
      run('nft', ['flush', 'chain', 'inet', 'herdr_smoke', 'output']);
      const started = Date.now();
      if (expectedCleanup) {
        await until(() => sockets().length === 0 && connections.every(({ ids }) => ids.every((pid) => !alive(pid))), 75000, 'all bridges AND SSH sessions reclaimed');
        console.log(`PASS after: 3/3 bridges and SSH sessions reclaimed in ${((Date.now() - started) / 1000).toFixed(1)}s`);
      } else {
        await delay(65000);
        if (sockets().length !== 3 || !connections.every(({ ids }) => ids.every(alive))) throw new Error('baseline did not retain the abandoned connections');
        console.log('RED before: 3/3 bridges and SSH sessions still retained after 65s');
        // Only these recorded disposable bridge PIDs are terminated, never sshd or a main server.
        for (const { ids } of connections) process.kill(ids[0], 'SIGTERM');
        await until(() => sockets().length === 0, 5000, 'baseline cleanup');
      }
      for (const { ids } of connections) bridges.delete(ids[0]);
      if (server.exitCode !== null || sshd.exitCode !== null) throw new Error('server died');
      run(binary, ['--session', 'ssh-smoke', 'api', 'snapshot'], env);
      console.log(`${label}: isolated Herdr server still responds`);
      server.kill('SIGTERM');
      await until(() => server.exitCode !== null, 5000, 'isolated server shutdown');
    }
    success = true;
    console.log('PASS: real SSH + Herdr, lost teardown, three concurrent connections, server survival');
  } finally {
    for (const pid of bridges) { try { process.kill(pid, 'SIGTERM'); } catch {} }
    for (const child of children.reverse()) { if (child.exitCode === null && child.signalCode === null) child.kill('SIGTERM'); }
    await delay(300);
    for (const child of children) { if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL'); }
    if (success) rmSync(root, { recursive: true, force: true });
    else console.error(`Smoke evidence retained at ${root}`);
  }
}

const args = process.argv.slice(2);
if (args[0] !== '--isolated') {
  if (args.length !== 2 || process.platform !== 'linux') {
    throw new Error('usage: bun scripts/smoke_ssh_bridge_liveness.mjs <before-binary> <after-binary> (Linux; requires unshare, nft, ss, sshd, ssh-keygen)');
  }
  const child = spawn('unshare', ['-Ucn', '--keep-caps', process.execPath, fileURLToPath(import.meta.url), '--isolated', ...args.map((path) => resolve(path))], { stdio: 'inherit' });
  child.on('exit', (code) => process.exit(code ?? 1));
} else {
  if (readlinkSync('/proc/self/ns/net') === readlinkSync('/proc/1/ns/net')) throw new Error('refusing host network namespace');
  await smoke(args[1], args[2]);
}
