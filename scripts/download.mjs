import { createHash, randomBytes } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { chmod, copyFile, link, mkdir, rename, rm, stat, writeFile } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { gunzipSync, inflateRawSync } from 'node:zlib';

const NAME = 'ds-mcp';
const REPO = `stubbedev/${NAME}`;

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const pkg = JSON.parse(readFileSync(join(root, 'package.json'), 'utf-8'));

const TARGETS = {
  'linux:x64': 'x86_64-unknown-linux-gnu',
  'linux:arm64': 'aarch64-unknown-linux-gnu',
  'darwin:arm64': 'aarch64-apple-darwin',
  'win32:x64': 'x86_64-pc-windows-msvc',
};

const windows = process.platform === 'win32';
const exe = windows ? '.exe' : '';

export async function ensureBinary() {
  const dest = join(root, 'bin', `${NAME}-native${exe}`);
  if (await present(dest)) return dest;
  const cached = join(cacheDir(), `${NAME}_${target()}${exe}`);
  if (!(await present(cached))) await fetchInto(cached);
  await place(cached, dest);
  return dest;
}

function target() {
  const key = `${process.platform}:${process.arch}`;
  const t = TARGETS[key];
  if (!t) {
    throw new Error(
      `Unsupported platform ${key}. Build from source (https://github.com/${REPO}#install) and set DS_MCP_BINARY.`,
    );
  }
  if (process.platform === 'linux' && !process.report?.getReport().header.glibcVersionRuntime) {
    throw new Error(
      `No prebuilt binary for musl linux. Build from source (https://github.com/${REPO}#install) and set DS_MCP_BINARY.`,
    );
  }
  return t;
}

async function fetchInto(cached) {
  const tag = `v${pkg.version}`;
  const archive = `${NAME}_${tag}_${target()}${windows ? '.zip' : '.tar.gz'}`;
  const data = await fetchRelease(tag, archive);
  await store(windows ? unzip(data, `${NAME}${exe}`) : untar(data, NAME), cached);
}

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

function cacheDir() {
  return join(cacheRoot(), NAME, pkg.version);
}

function cacheRoot() {
  if (process.env.XDG_CACHE_HOME) return process.env.XDG_CACHE_HOME;
  const [variable, ...suffix] =
    { win32: ['LOCALAPPDATA'], darwin: ['HOME', 'Library', 'Caches'] }[process.platform] ?? ['HOME', '.cache'];
  const base = process.env[variable];
  if (!base) {
    throw new Error(`Cannot locate a cache directory: neither XDG_CACHE_HOME nor ${variable} is set`);
  }
  return join(base, ...suffix);
}

async function present(path) {
  try {
    const s = await stat(path);
    return s.isFile() && s.size > 0;
  } catch (err) {
    if (err.code === 'ENOENT') return false;
    throw err;
  }
}

async function place(cached, dest) {
  const tmp = await tmpBeside(dest);
  try {
    await link(cached, tmp).catch(() => copyFile(cached, tmp));
    await commit(tmp, dest);
  } finally {
    await rm(tmp, { force: true });
  }
}

async function commit(tmp, dest) {
  await chmod(tmp, 0o755);
  await rename(tmp, dest);
}

async function tmpBeside(file) {
  await mkdir(dirname(file), { recursive: true });
  return `${file}.${randomBytes(6).toString('hex')}.tmp`;
}

async function fetchRelease(tag, asset) {
  const base = `https://github.com/${REPO}/releases/download/${tag}`;
  const sums = (await get(`${base}/checksums.txt`)).toString('utf8');
  const want = sums
    .split('\n')
    .map((line) => line.trim().split(/\s+\*?/))
    .find(([, file]) => file === asset)?.[0];
  if (!want) throw new Error(`${asset} is not listed in ${tag} checksums.txt`);
  const data = await get(`${base}/${asset}`);
  if (createHash('sha256').update(data).digest('hex') !== want) {
    throw new Error(`Checksum mismatch for ${asset} from ${tag}`);
  }
  return data;
}

async function get(url) {
  const res = await fetch(url, { redirect: 'follow' });
  if (!res.ok) throw new Error(`Failed to download ${url}: HTTP ${res.status}`);
  return Buffer.from(await res.arrayBuffer());
}

async function store(data, cached) {
  const tmp = await tmpBeside(cached);
  try {
    await writeFile(tmp, data);
    await commit(tmp, cached);
  } finally {
    await rm(tmp, { force: true });
  }
}
