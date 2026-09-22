import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { PassThrough } from 'node:stream';
import { createRequire } from 'node:module';
import { test } from 'node:test';

const require = createRequire(import.meta.url);
const { RustBridge } = require('./bridge.cjs');
const { assetPath, serveStatic } = require('./static.cjs');

function fakeEngine() {
  const child = new EventEmitter();
  child.exitCode = null;
  child.signalCode = null;
  child.stdin = new PassThrough();
  child.stdout = new PassThrough();
  child.stderr = new PassThrough();
  child.kill = () => { child.emit('exit', 0, 'SIGTERM'); return true; };
  return child;
}

test('bridge sends structured commands and forwards progress without mixing responses', async () => {
  const child = fakeEngine();
  let argumentsSeen;
  const bridge = new RustBridge('/tmp/converter', {
    spawnProcess: (_binary, args, options) => {
      argumentsSeen = { args, options };
      return child;
    },
  });
  const progress = [];
  bridge.on('progress', (event) => progress.push(event));
  child.stdin.once('data', (buffer) => {
    const request = JSON.parse(buffer.toString());
    assert.deepEqual(request.command, 'start_job');
    assert.deepEqual(request.args, { request: { kind: 'pdf_to_txt' } });
    child.stdout.write(JSON.stringify({ event: 'conversion://progress', payload: { job_id: 'job_1', seq: 1, state: 'queued' } }) + '\n');
    child.stdout.write(JSON.stringify({ id: request.id, ok: true, result: 'job_1' }) + '\n');
  });
  assert.equal(await bridge.invoke('start_job', { request: { kind: 'pdf_to_txt' } }), 'job_1');
  assert.equal(progress[0].job_id, 'job_1');
  assert.deepEqual(argumentsSeen.args, ['--electron-bridge']);
  assert.equal(argumentsSeen.options.shell, false);
  child.stdin.once('finish', () => child.emit('exit', 0, null));
  await bridge.close();
});

test('bridge forwards the controlled runtime environment to the Rust process', async () => {
  const child = fakeEngine();
  let spawnedWith;
  const env = { MINIMALPDF_OFFICE_EXECUTABLE: '/tmp/resources/office/soffice' };
  const bridge = new RustBridge('/tmp/converter', {
    env,
    spawnProcess: (_binary, _args, options) => {
      spawnedWith = options;
      return child;
    },
  });
  assert.equal(spawnedWith.env, env);
  assert.equal(spawnedWith.shell, false);
  child.stdin.once('finish', () => child.emit('exit', 0, null));
  await bridge.close();
});

test('bridge rejects invalid commands and closes on malformed protocol', async () => {
  const child = fakeEngine();
  const bridge = new RustBridge('/tmp/converter', { spawnProcess: () => child });
  await assert.rejects(bridge.invoke('arbitrary_command'), /ENGINE_COMMAND_INVALID/);
  const pending = bridge.invoke('get_self_check');
  child.stdout.write('not-json\n');
  await assert.rejects(pending, /ENGINE_PROTOCOL_INVALID/);
  await assert.rejects(bridge.invoke('get_self_check'), /ENGINE_UNAVAILABLE/);
});

test('normal shutdown closes stdin before terminating the engine', async () => {
  const child = fakeEngine();
  let wasKilled = false;
  child.kill = () => { wasKilled = true; child.emit('exit', 0, 'SIGTERM'); return true; };
  const bridge = new RustBridge('/tmp/converter', { spawnProcess: () => child });
  child.stdin.once('finish', () => child.emit('exit', 0, null));
  await bridge.close();
  assert.equal(wasKilled, false);
  assert.equal(child.stdin.writableEnded, true);
});

test('unexpected engine exit rejects pending work and emits one termination event', async () => {
  const child = fakeEngine();
  const bridge = new RustBridge('/tmp/converter', { spawnProcess: () => child });
  const terminations = [];
  bridge.on('terminated', (code) => terminations.push(code));
  const pending = bridge.invoke('get_self_check');
  child.emit('exit', 1, null);
  await assert.rejects(pending, /ENGINE_UNAVAILABLE/);
  assert.deepEqual(terminations, ['ENGINE_UNAVAILABLE']);
});

test('protocol limit applies to each line rather than a batch of valid lines', async () => {
  const child = fakeEngine();
  const bridge = new RustBridge('/tmp/converter', { spawnProcess: () => child });
  const padding = 'x'.repeat(180_000);
  bridge.onData(Array.from({ length: 6 }, (_, index) => JSON.stringify({ id: `orphan-${index}`, ok: true, result: padding }) + '\n').join(''));
  child.stdin.once('data', (buffer) => {
    const request = JSON.parse(buffer.toString());
    child.stdout.write(JSON.stringify({ id: request.id, ok: true, result: [] }) + '\n');
  });
  assert.deepEqual(await bridge.invoke('get_self_check'), []);
  child.stdin.once('finish', () => child.emit('exit', 0, null));
  await bridge.close();
});

test('app protocol only resolves resources inside its static export', async () => {
  assert.equal(assetPath('/tmp/pdf-app/out', 'app://local/'), '/tmp/pdf-app/out/index.html');
  assert.equal(assetPath('/tmp/pdf-app/out', 'app://other/'), null);
  assert.equal(assetPath('/tmp/pdf-app/out', 'app://local/%2e%2e%2fprivate'), null);
  assert.equal(assetPath('/tmp/pdf-app/out', 'app://local/%00'), null);
  const response = await serveStatic('/tmp/pdf-app/out', 'app://local/not-present.js');
  assert.equal(response.status, 404);
});
