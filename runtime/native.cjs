'use strict';
const { spawn } = require('node:child_process');
const path = require('node:path');
const readline = require('node:readline');
const binary = path.resolve(
  __dirname,
  '../native/target/release',
  'super-agent-core' + (process.platform === 'win32' ? '.exe' : '')
);

class Native {
  constructor() {
    this.pending = new Map();
    this.seq = 0;
  }
  open() {
    if (this.child) return;
    if (this.failed) throw this.failed;
    const child = spawn(binary, [], { stdio: ['pipe', 'pipe', 'inherit'], windowsHide: true });
    this.child = child;
    const fail = (error) => {
      this.failed = error;
      for (const { reject } of this.pending.values()) reject(error);
      this.pending.clear();
    };
    readline.createInterface({ input: child.stdout, crlfDelay: Infinity }).on('line', (line) => {
      try {
        const { id, value, error } = JSON.parse(line);
        const request = this.pending.get(id);
        if (!request) throw new Error('原生响应无法匹配请求');
        this.pending.delete(id);
        if (error) request.reject(new Error(error));
        else request.resolve(value);
      } catch (error) {
        fail(error);
        child.kill();
      }
    });
    child.once('error', fail);
    child.stdin.on('error', fail);
    this.ended = new Promise((resolve) =>
      child.once('close', (code) => {
        fail(new Error('Rust 服务已关闭，退出码 ' + code));
        this.child = null;
        resolve();
      })
    );
  }
  async call(method, params) {
    this.open();
    const id = ++this.seq;
    const line = JSON.stringify({ id, method, params }) + '\n';
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.child.stdin.write(line, 'utf8', (err) => {
        if (err) {
          this.pending.delete(id);
          reject(err);
        }
      });
    });
  }
  async close() {
    this.child?.stdin.end();
    await this.ended;
  }
}
module.exports = { Native };
