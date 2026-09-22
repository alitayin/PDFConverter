'use strict';

const { spawn } = require('node:child_process');
const { EventEmitter } = require('node:events');
const { randomUUID } = require('node:crypto');
const { isAbsolute } = require('node:path');

const COMMANDS = new Set([
  'get_self_check',
  'get_diagnostic_info',
  'inspect_input_files',
  'start_job',
  'cancel_job',
]);
const MAX_LINE_BYTES = 1024 * 1024;

class RustBridge extends EventEmitter {
  constructor(binary, { args = [], env, spawnProcess = spawn, timeoutMs = 30_000 } = {}) {
    super();
    if (!isAbsolute(binary)) throw new Error('ENGINE_PATH_INVALID');
    this.pending = new Map();
    this.buffer = '';
    this.closed = false;
    this.timeoutMs = timeoutMs;
    this.child = spawnProcess(binary, ['--electron-bridge', ...args], {
      stdio: ['pipe', 'pipe', 'pipe'],
      shell: false,
      windowsHide: true,
      ...(env ? { env } : {}),
    });
    this.child.stdout.setEncoding('utf8');
    this.child.stdout.on('data', (chunk) => this.onData(chunk));
    this.child.stderr.resume();
    this.child.on('error', () => {
      this.finishShutdown?.();
      this.fail('ENGINE_NOT_INSTALLED');
    });
    this.child.on('exit', () => {
      this.finishShutdown?.();
      this.fail('ENGINE_UNAVAILABLE');
    });
    this.child.stdin.on('error', () => this.fail('ENGINE_UNAVAILABLE'));
  }

  onData(chunk) {
    this.buffer += chunk;
    let newline = this.buffer.indexOf('\n');
    while (newline !== -1) {
      const line = this.buffer.slice(0, newline);
      this.buffer = this.buffer.slice(newline + 1);
      if (Buffer.byteLength(line, 'utf8') + 1 > MAX_LINE_BYTES) {
        this.fail('ENGINE_PROTOCOL_INVALID');
        this.child.kill('SIGTERM');
        return;
      }
      try {
        this.onLine(JSON.parse(line));
      } catch {
        this.fail('ENGINE_PROTOCOL_INVALID');
        this.child.kill('SIGTERM');
        return;
      }
      if (Buffer.byteLength(this.buffer, 'utf8') > MAX_LINE_BYTES) {
        this.fail('ENGINE_PROTOCOL_INVALID');
        this.child.kill('SIGTERM');
        return;
      }
      newline = this.buffer.indexOf('\n');
    }
  }

  onLine(message) {
    if (!message || typeof message !== 'object') throw new Error('ENGINE_PROTOCOL_INVALID');
    if (message.event === 'conversion://progress') {
      if (typeof message.payload?.job_id !== 'string' || !Number.isSafeInteger(message.payload?.seq)) {
        throw new Error('ENGINE_PROTOCOL_INVALID');
      }
      this.emit('progress', message.payload);
      return;
    }
    if (typeof message.id !== 'string' || typeof message.ok !== 'boolean') {
      throw new Error('ENGINE_PROTOCOL_INVALID');
    }
    const request = this.pending.get(message.id);
    if (!request) return;
    clearTimeout(request.timeout);
    this.pending.delete(message.id);
    if (message.ok) request.resolve(message.result);
    else request.reject(new Error(typeof message.error === 'string' ? message.error : 'ENGINE_UNAVAILABLE'));
  }

  invoke(command, args = {}) {
    if (!COMMANDS.has(command)) return Promise.reject(new Error('ENGINE_COMMAND_INVALID'));
    if (this.closed) return Promise.reject(new Error('ENGINE_UNAVAILABLE'));
    if (this.pending.size >= 64) return Promise.reject(new Error('ENGINE_BUSY'));
    const id = randomUUID();
    const line = JSON.stringify({ id, command, args }) + '\n';
    if (Buffer.byteLength(line, 'utf8') > MAX_LINE_BYTES) {
      return Promise.reject(new Error('ENGINE_REQUEST_TOO_LARGE'));
    }
    return new Promise((resolve, reject) => {
      const timeout = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error('ENGINE_TIMEOUT'));
      }, this.timeoutMs);
      this.pending.set(id, { resolve, reject, timeout });
      this.child.stdin.write(line, (error) => {
        if (!error || !this.pending.has(id)) return;
        clearTimeout(timeout);
        this.pending.delete(id);
        reject(new Error('ENGINE_UNAVAILABLE'));
      });
    });
  }

  fail(code) {
    if (this.closed) return;
    this.closed = true;
    for (const request of this.pending.values()) {
      clearTimeout(request.timeout);
      request.reject(new Error(code));
    }
    this.pending.clear();
    this.emit('terminated', code);
  }

  close() {
    if (this.shutdown) return this.shutdown;
    this.fail('ENGINE_UNAVAILABLE');
    this.shutdown = new Promise((resolve) => {
      this.finishShutdown = () => {
        clearTimeout(this.shutdownTimer);
        this.finishShutdown = undefined;
        resolve();
      };
      if (this.child.exitCode !== null || this.child.signalCode !== null) {
        this.finishShutdown();
        return;
      }
      this.shutdownTimer = setTimeout(() => {
        this.child.kill('SIGTERM');
        this.finishShutdown?.();
      }, 10_000);
      this.child.stdin.end();
    });
    return this.shutdown;
  }
}

module.exports = { RustBridge };
