'use strict';
const fs = require('node:fs');
const path = require('node:path');
const { createHash } = require('node:crypto');

// Only public file metadata feeds the revision. stat({bigint:true}) preserves
// nanosecond timestamps, and inode/ctime catch ordinary atomic replacements.
function stamp(file) {
  const absolute = path.resolve(file);
  try {
    const st = fs.statSync(absolute, { bigint: true });
    return [absolute, String(st.size), String(st.mtimeNs), String(st.ctimeNs), String(st.ino)];
  } catch (error) {
    return [absolute, error.code || 'unreadable'];
  }
}

function fingerprint(paths) {
  return createHash('sha256')
    .update(JSON.stringify(paths.filter(Boolean).map(stamp)))
    .digest('hex');
}

// A locator/TOML may point outside its own directory. Re-parse only when the
// source metadata changes, retaining just its path-valued field. Never cache a
// whole config with tokens here or send it to a renderer.
const pointers = new Map();
function pathField(slot, file, read) {
  const meta = JSON.stringify(stamp(file));
  const saved = pointers.get(slot);
  if (saved?.meta === meta) return saved.value;
  let value;
  try {
    const result = read();
    if (typeof result === 'string') value = result;
  } catch {
    /* Keep watching the missing or invalid source for repair. */
  }
  pointers.set(slot, { meta, value });
  return value;
}
module.exports = { fingerprint, pathField };
