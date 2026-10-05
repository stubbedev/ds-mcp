// Resolves (and, if needed, downloads) the prebuilt binary that matches the
// current platform. Shared by the postinstall script and the CLI launcher so a
// failed install (e.g. offline) self-heals on first run.
//
// The release ships each binary inside an archive (.tar.gz, or .zip on
// Windows) listed in checksums.txt; the archive is verified against that list
// and the one file in it unpacked here, with no tar/unzip dependency.
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { chmod, mkdir, rename, rm, stat, writeFile } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { gunzipSync, inflateRawSync } from 'node:zlib';

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, '..');
const binDir = join(root, 'bin');

const pkg = JSON.parse(readFileSync(join(root, 'package.json'), 'utf-8'));
const REPO = 'stubbedev/ds-mcp';

// Map Node's platform/arch onto the release's Rust target triples.
const TARGETS = {
  'linux:x64': 'x86_64-unknown-linux-gnu',
  'linux:arm64': 'aarch64-unknown-linux-gnu',
  'darwin:x64': 'x86_64-apple-darwin',
  'darwin:arm64': 'aarch64-apple-darwin',
  'win32:x64': 'x86_64-pc-windows-msvc',
};

function target() {
  const key = `${process.platform}:${process.arch}`;
  const t = TARGETS[key];
  if (!t) {
    throw new Error(
      `Unsupported platform ${key}. Build from source (https://github.com/${REPO}#install) and set DS_MCP_BINARY.`,
    );
  }
  // The linux binaries link glibc; on musl (Alpine) they would not start.
  if (process.platform === 'linux' && !process.report?.getReport().header.glibcVersionRuntime) {
    throw new Error(
      `No prebuilt binary for musl linux. Build from source (https://github.com/${REPO}#install) and set DS_MCP_BINARY.`,
    );
  }
  return t;
}

const exe = () => (process.platform === 'win32' ? '.exe' : '');

export function binaryPath() {
  return join(binDir, `ds-mcp-native${exe()}`);
}

async function exists(path) {
  try {
    const s = await stat(path);
    return s.isFile() && s.size > 0;
  } catch {
    return false;
  }
}

async function get(url) {
  const res = await fetch(url, { redirect: 'follow' });
  if (!res.ok) throw new Error(`Failed to download ${url}: HTTP ${res.status}`);
  return Buffer.from(await res.arrayBuffer());
}

// Pull one regular file out of a gzipped tarball.
function untar(gz, name) {
  const tar = gunzipSync(gz);
  for (let off = 0; off + 512 <= tar.length; ) {
    const header = tar.subarray(off, off + 512);
    if (header.every((b) => b === 0)) break;
    const field = (start, len) => header.toString('utf8', start, start + len).replace(/\0.*$/s, '');
    const size = parseInt(field(124, 12).trim() || '0', 8);
    off += 512;
    if (field(0, 100).replace(/^\.\//, '') === name) return tar.subarray(off, off + size);
    off += Math.ceil(size / 512) * 512;
  }
  throw new Error(`${name} not found in release archive`);
}

// Pull one stored or deflated file out of a zip via its central directory.
function unzip(zip, name) {
  const eocd = zip.lastIndexOf(Buffer.from([0x50, 0x4b, 0x05, 0x06]));
  if (eocd < 0) throw new Error('release archive is not a zip');
  let p = zip.readUInt32LE(eocd + 16);
  for (let i = zip.readUInt16LE(eocd + 10); i > 0; i--) {
    const method = zip.readUInt16LE(p + 10);
    const size = zip.readUInt32LE(p + 20);
    const nameLen = zip.readUInt16LE(p + 28);
    const local = zip.readUInt32LE(p + 42);
    if (zip.toString('utf8', p + 46, p + 46 + nameLen) === name) {
      const start = local + 30 + zip.readUInt16LE(local + 26) + zip.readUInt16LE(local + 28);
      const data = zip.subarray(start, start + size);
      if (method === 0) return data;
      if (method === 8) return inflateRawSync(data);
      throw new Error(`unsupported zip compression method ${method}`);
    }
    p += 46 + nameLen + zip.readUInt16LE(p + 30) + zip.readUInt16LE(p + 32);
  }
  throw new Error(`${name} not found in release archive`);
}

// ensureBinary returns the path to the platform binary, downloading it from the
// matching GitHub release if it is not already present.
export async function ensureBinary() {
  const dest = binaryPath();
  if (await exists(dest)) return dest;

  const t = target();
  const tag = `v${pkg.version}`;
  const archive = `ds-mcp_${tag}_${t}${process.platform === 'win32' ? '.zip' : '.tar.gz'}`;
  const base = `https://github.com/${REPO}/releases/download/${tag}`;

  const sums = (await get(`${base}/checksums.txt`)).toString('utf8');
  const want = sums
    .split('\n')
    .map((line) => line.trim().split(/\s+\*?/))
    .find(([, file]) => file === archive)?.[0];
  if (!want) throw new Error(`${archive} is not listed in ${tag} checksums.txt`);

  const data = await get(`${base}/${archive}`);
  if (createHash('sha256').update(data).digest('hex') !== want) {
    throw new Error(`Checksum mismatch for ${archive}`);
  }
  const bin = process.platform === 'win32' ? unzip(data, 'ds-mcp.exe') : untar(data, 'ds-mcp');

  await mkdir(binDir, { recursive: true });
  const tmp = `${dest}.download`;
  await writeFile(tmp, bin);
  await chmod(tmp, 0o755).catch(() => {});
  await rm(dest, { force: true });
  await rename(tmp, dest);
  return dest;
}
