#!/usr/bin/env node

import { createHash } from 'node:crypto'
import { lstat, readFile, readdir } from 'node:fs/promises'
import path from 'node:path'

const packageRoot = process.argv[2] ? path.resolve(process.argv[2]) : ''
if (!packageRoot) {
  throw new Error('Usage: node scripts/verify-release-package.mjs <extracted-package-directory>')
}

const manifest = JSON.parse(await readFile(path.join(packageRoot, 'manifest.json'), 'utf8'))
const checksums = JSON.parse(await readFile(path.join(packageRoot, 'checksums.json'), 'utf8'))
if (
  manifest.manifestVersion !== 2
  || manifest.id !== 'translator'
  || !/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(-[0-9A-Za-z.-]+)?$/.test(manifest.version ?? '')
  || manifest.platform?.os !== 'windows'
  || manifest.platform?.architecture !== 'x86_64'
  || manifest.platform?.abi !== 'msvc'
) {
  throw new Error('Release package contains invalid translator manifest metadata.')
}
if (checksums.algorithm !== 'sha256' || !Array.isArray(checksums.files) || checksums.files.length === 0) {
  throw new Error('Release package contains an invalid checksums.json.')
}
for (const relativePath of [
  manifest.ui?.entry,
  manifest.backend?.entry,
  manifest.contract,
]) {
  if (typeof relativePath !== 'string') {
    throw new Error('Manifest is missing a package entry path.')
  }
}

async function collectFiles(directory, prefix = '') {
  const files = []
  const entries = await readdir(directory, { withFileTypes: true })
  entries.sort((left, right) => left.name.localeCompare(right.name))
  for (const entry of entries) {
    const relativePath = prefix ? prefix + '/' + entry.name : entry.name
    const absolutePath = path.join(directory, entry.name)
    const info = await lstat(absolutePath)
    if (info.isSymbolicLink()) {
      throw new Error('Release package contains a symbolic link: ' + relativePath)
    }
    if (entry.isDirectory()) {
      files.push(...await collectFiles(absolutePath, relativePath))
    } else if (entry.isFile()) {
      if (relativePath !== 'checksums.json') files.push(relativePath)
    } else {
      throw new Error('Release package contains an unsupported entry: ' + relativePath)
    }
  }
  return files
}

const actualFiles = (await collectFiles(packageRoot)).sort()
for (const relativePath of [
  manifest.ui.entry,
  manifest.backend.entry,
  manifest.contract,
]) {
  if (!actualFiles.includes(relativePath)) {
    throw new Error('Manifest references a missing package file: ' + relativePath)
  }
}
const listedFiles = checksums.files.map((entry) => entry.path)
if (listedFiles.some((relativePath) => (
  typeof relativePath !== 'string'
  || !/^[A-Za-z0-9_.-]+(?:\/[A-Za-z0-9_.-]+)*$/.test(relativePath)
  || relativePath.split('/').some((part) => part === '.' || part === '..')
))) {
  throw new Error('checksums.json contains an invalid package path.')
}
if (new Set(listedFiles).size !== listedFiles.length) {
  throw new Error('checksums.json contains duplicate paths.')
}
listedFiles.sort()
if (JSON.stringify(actualFiles) !== JSON.stringify(listedFiles)) {
  throw new Error('checksums.json does not cover exactly the package files.')
}

for (const entry of checksums.files) {
  if (!/^[a-f0-9]{64}$/.test(entry.sha256 ?? '')) {
    throw new Error('checksums.json contains an invalid SHA-256 value for ' + entry.path + '.')
  }
  const bytes = await readFile(path.join(packageRoot, ...entry.path.split('/')))
  const actualHash = createHash('sha256').update(bytes).digest('hex')
  if (actualHash !== entry.sha256) {
    throw new Error('Checksum mismatch for ' + entry.path + '.')
  }
}

console.log('Verified translator v' + manifest.version + ' package checksums.')
