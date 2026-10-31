#!/usr/bin/env node

/**
 * Omni MCP Native Runner
 * Automatically selects the appropriate precompiled binary for the host platform,
 * or downloads it from GitHub Releases if not already cached.
 */

const { spawn } = require('child_process');
const path = require('path');
const fs = require('fs');
const os = require('os');
const https = require('https');
const crypto = require('crypto');

const VERSION = require('../package.json').version;
const GITHUB_REPO = 'Rangow4562/omni-mcp-gateway';

function getPlatformBinaryName() {
  const platform = process.platform;
  const arch = process.arch;

  if (platform === 'win32' && arch === 'x64') {
    return 'omni-gateway-win32-x64.exe';
  } else if (platform === 'win32' && arch === 'arm64') {
    return 'omni-gateway-win32-arm64.exe';
  } else if (platform === 'darwin' && arch === 'arm64') {
    return 'omni-gateway-darwin-arm64';
  } else if (platform === 'darwin' && arch === 'x64') {
    return 'omni-gateway-darwin-x64';
  } else if (platform === 'linux' && arch === 'x64') {
    return 'omni-gateway-linux-x64';
  } else if (platform === 'linux' && arch === 'arm64') {
    return 'omni-gateway-linux-arm64';
  }
  return null;
}

function fetchBuffer(url) {
  return new Promise((resolve, reject) => {
    https.get(url, (res) => {
      if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
        return fetchBuffer(res.headers.location).then(resolve).catch(reject);
      }
      if (res.statusCode !== 200) {
        return reject(new Error(`HTTP ${res.statusCode}`));
      }
      const chunks = [];
      res.on('data', chunk => chunks.push(chunk));
      res.on('end', () => resolve(Buffer.concat(chunks)));
      res.on('error', reject);
    }).on('error', reject);
  });
}

function downloadFileWithHash(url, tempDest) {
  return new Promise((resolve, reject) => {
    https.get(url, (res) => {
      if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
        return downloadFileWithHash(res.headers.location, tempDest).then(resolve).catch(reject);
      }
      if (res.statusCode !== 200) {
        return reject(new Error(`Failed to download binary: HTTP ${res.statusCode}`));
      }
      const hash = crypto.createHash('sha256');
      const file = fs.createWriteStream(tempDest);
      res.on('data', chunk => hash.update(chunk));
      res.pipe(file);
      file.on('finish', () => {
        file.close(() => {
          resolve(hash.digest('hex'));
        });
      });
      file.on('error', (err) => {
        fs.unlink(tempDest, () => {});
        reject(err);
      });
    }).on('error', (err) => {
      fs.unlink(tempDest, () => {});
      reject(err);
    });
  });
}

async function downloadAndVerify(url, cachedBinary, binaryFileName) {
  const tempBinary = `${cachedBinary}.${Date.now()}.tmp`;
  try {
    const computedHash = await downloadFileWithHash(url, tempBinary);

    // Optional verification against SHA256SUMS if published
    const checksumsUrl = `https://github.com/${GITHUB_REPO}/releases/download/v${VERSION}/SHA256SUMS`;
    try {
      const checksumsBuf = await fetchBuffer(checksumsUrl);
      const checksumsText = checksumsBuf.toString('utf8');
      const lines = checksumsText.split('\n');
      for (const line of lines) {
        const parts = line.trim().split(/\s+/);
        if (parts.length >= 2) {
          const expectedHash = parts[0].toLowerCase();
          const filename = parts[1].replace(/^\*/, '');
          if (filename === binaryFileName) {
            if (computedHash.toLowerCase() !== expectedHash) {
              throw new Error(`SHA-256 verification failed for ${binaryFileName}! Expected: ${expectedHash}, computed: ${computedHash}`);
            }
            process.stderr.write(`[omni-mcp] SHA-256 verified successfully (${computedHash.slice(0, 16)}...).\n`);
            break;
          }
        }
      }
    } catch (csErr) {
      if (csErr.message.includes('SHA-256 verification failed')) {
        throw csErr;
      }
    }

    if (fs.existsSync(cachedBinary)) {
      try { fs.unlinkSync(cachedBinary); } catch (_) {}
    }
    fs.renameSync(tempBinary, cachedBinary);
  } catch (err) {
    if (fs.existsSync(tempBinary)) {
      try { fs.unlinkSync(tempBinary); } catch (_) {}
    }
    throw err;
  }
}

async function getBinaryPath() {
  const binaryFileName = getPlatformBinaryName();
  if (!binaryFileName) {
    console.error(`[omni-mcp] Unsupported platform/architecture: ${process.platform}-${process.arch}`);
    process.exit(1);
  }

  // 1. Check local prebuilt inside npm package (e.g. bin/<name>)
  const bundledPath = path.join(__dirname, binaryFileName);
  if (fs.existsSync(bundledPath)) {
    return bundledPath;
  }

  // 2. Check local cargo release target (during development or local build)
  const isWindows = process.platform === 'win32';
  const targetExe = isWindows ? 'omni-gateway.exe' : 'omni-gateway';
  const cargoReleasePath = path.join(__dirname, '..', 'target', 'release', targetExe);
  if (fs.existsSync(cargoReleasePath)) {
    return cargoReleasePath;
  }

  // 3. Check persistent user cache directory
  const cacheDir = path.join(os.homedir(), '.cache', 'omni-mcp', `v${VERSION}`);
  const cachedBinary = path.join(cacheDir, binaryFileName);
  if (fs.existsSync(cachedBinary)) {
    return cachedBinary;
  }

  // 4. Download from GitHub Releases
  const downloadUrl = `https://github.com/${GITHUB_REPO}/releases/download/v${VERSION}/${binaryFileName}`;
  fs.mkdirSync(cacheDir, { recursive: true });

  process.stderr.write(`[omni-mcp] Downloading native binary for ${process.platform}-${process.arch} from GitHub...\n`);
  await downloadAndVerify(downloadUrl, cachedBinary, binaryFileName);

  if (process.platform !== 'win32') {
    fs.chmodSync(cachedBinary, 0o755);
  }

  return cachedBinary;
}

async function main() {
  try {
    const binPath = await getBinaryPath();
    const args = process.argv.slice(2);

    const child = spawn(binPath, args, {
      stdio: 'inherit',
      env: process.env,
    });

    child.on('exit', (code, signal) => {
      if (signal) {
        process.kill(process.pid, signal);
      } else {
        process.exit(code || 0);
      }
    });

    child.on('error', (err) => {
      console.error('[omni-mcp] Failed to spawn binary:', err);
      process.exit(1);
    });
  } catch (err) {
    console.error('[omni-mcp] Error initializing native runner:', err.message);
    process.exit(1);
  }
}

main();
