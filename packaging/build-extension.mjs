import { spawnSync } from 'node:child_process';
import { createHash, createPublicKey } from 'node:crypto';
import { cpSync, existsSync, mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const updateUrl = 'https://grandlaite.github.io/YouTubeDream/updates.xml';
const args = process.argv.slice(2);
const option = (name) => {
  const index = args.indexOf(name);
  return index >= 0 ? args[index + 1] : undefined;
};
const expectedId = option('--expected-id') || 'kimlopaoiianjkblnjfakfjjilaojkjc';
const keyPath = option('--key');
const edgePath = option('--edge');
const makensis = option('--makensis');
const extensionDir = path.resolve(root, option('--extension-dir') || 'extension');
const outDir = path.resolve(root, option('--out') || 'target/extension');
const skipInstaller = args.includes('--skip-installer');
const hostPath = path.resolve(root, option('--host') || 'lader-host.exe');

if (!keyPath) throw new Error('Укажите PEM-ключ: --key /path/to/lader-extension.pem');
if (!existsSync(keyPath)) throw new Error(`Ключ не найден: ${keyPath}`);
if (!skipInstaller && !existsSync(hostPath)) {
  throw new Error(`Помощник не найден: ${hostPath}. Сначала выполните make host`);
}

const manifest = JSON.parse(readFileSync(path.join(extensionDir, 'manifest.json'), 'utf8'));
const publicDer = createPublicKey(readFileSync(keyPath)).export({ type: 'spki', format: 'der' });
const publicKey = Buffer.from(publicDer).toString('base64');
if (manifest.key !== publicKey) {
  throw new Error('Закрытый ключ не соответствует публичному key в extension/manifest.json');
}
const id = [...createHash('sha256').update(publicDer).digest().subarray(0, 16)]
  .map((byte) => String.fromCharCode(97 + (byte >> 4)) + String.fromCharCode(97 + (byte & 15)))
  .join('');
if (id !== expectedId) throw new Error(`Ключ даёт неожиданный ID расширения: ${id}`);

mkdirSync(outDir, { recursive: true });
const packageDir = path.join(outDir, 'package');
const packedCrx = `${packageDir}.crx`;
const profileDir = path.join(outDir, 'edge-profile');
rmSync(packageDir, { recursive: true, force: true });
rmSync(packedCrx, { force: true });
rmSync(profileDir, { recursive: true, force: true });
cpSync(extensionDir, packageDir, { recursive: true });
manifest.update_url = updateUrl;
writeFileSync(path.join(packageDir, 'manifest.json'), `${JSON.stringify(manifest, null, 2)}\n`);

function findEdge() {
  if (edgePath) return edgePath;
  const candidates = [
    process.env.EDGE_PATH,
    process.env['PROGRAMFILES(X86)'] && path.join(process.env['PROGRAMFILES(X86)'], 'Microsoft/Edge/Application/msedge.exe'),
    process.env.PROGRAMFILES && path.join(process.env.PROGRAMFILES, 'Microsoft/Edge/Application/msedge.exe'),
    process.env.LOCALAPPDATA && path.join(process.env.LOCALAPPDATA, 'Microsoft/Edge/Application/msedge.exe'),
    '/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge',
  ].filter(Boolean);
  const installed = candidates.find((candidate) => existsSync(candidate));
  if (installed) return installed;
  return process.platform === 'win32' ? 'msedge' : 'microsoft-edge';
}

const edge = findEdge();
const packed = spawnSync(edge, [
  `--pack-extension=${packageDir}`,
  `--pack-extension-key=${path.resolve(keyPath)}`,
  '--no-message-box',
  `--user-data-dir=${profileDir}`,
], { stdio: 'inherit' });
rmSync(profileDir, { recursive: true, force: true });
if (packed.error) throw packed.error;
if (packed.status !== 0 || !existsSync(packedCrx)) {
  throw new Error(`Edge не собрал CRX (код ${packed.status ?? 'неизвестен'})`);
}

const baseName = `lader-extension_${manifest.version}`;
const crxPath = path.join(outDir, `${baseName}.crx`);
rmSync(crxPath, { force: true });
renameSync(packedCrx, crxPath);
const codebase = `https://github.com/GrandLaite/YouTubeDream/releases/download/v${manifest.version}/${baseName}.crx`;
const updates = `<?xml version="1.0" encoding="UTF-8"?>\n` +
  `<gupdate xmlns="http://www.google.com/update2/response" protocol="2.0">\n` +
  `  <app appid="${id}">\n` +
  `    <updatecheck codebase="${codebase}" version="${manifest.version}" />\n` +
  `  </app>\n</gupdate>\n`;
const updatesPath = path.join(outDir, 'updates.xml');
writeFileSync(updatesPath, updates);

if (!skipInstaller) {
  const setupPath = path.join(outDir, `${baseName}-setup.exe`);
  const built = spawnSync(makensis || 'makensis', [
    '/V4',
    `/DVERSION=${manifest.version}`,
    `/DEXTENSION_ID=${id}`,
    `/DUPDATE_URL=${updateUrl}`,
    `/DOUT_FILE=${setupPath}`,
    `/DHOST_FILE=${hostPath}`,
    path.join(root, 'packaging/nsis/extension.nsi'),
  ], { stdio: 'inherit' });
  if (built.error) throw built.error;
  if (built.status !== 0 || !existsSync(setupPath)) throw new Error('NSIS не собрал установщик расширения');
  console.log(`Установщик с помощником: ${setupPath}`);
}

console.log(`CRX: ${crxPath}`);
console.log(`Update manifest: ${updatesPath}`);
