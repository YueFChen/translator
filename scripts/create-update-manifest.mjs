#!/usr/bin/env node

import {
  createHash,
  createPrivateKey,
  createPublicKey,
  sign,
  verify,
} from 'node:crypto'
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { basename, dirname } from 'node:path'

const [, , manifestPath, packagePath, repository, tag, outputPath] = process.argv
if (!manifestPath || !packagePath || !repository || !tag || !outputPath) {
  throw new Error(
    'Usage: node scripts/create-update-manifest.mjs <manifest.json> <package.wplug> <owner/repo> <tag> <output.json>',
  )
}

const encodedSeed = (process.env.PLUGIN_UPDATE_SIGNING_KEY ?? '').trim()
const seed = Buffer.from(encodedSeed, 'base64')
if (seed.length !== 32 || seed.toString('base64') !== encodedSeed) {
  throw new Error(
    'PLUGIN_UPDATE_SIGNING_KEY must be the canonical base64 encoding of a 32-byte Ed25519 seed.',
  )
}

const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'))
if (!/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(repository)) {
  throw new Error('Repository must be an owner/repository pair.')
}
if (
  !/^[a-z][a-z0-9_-]{0,63}$/.test(manifest.id ?? '')
  || !/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(-[0-9A-Za-z.-]+)?$/.test(manifest.version ?? '')
  || !manifest.hostCompatibility
  || !manifest.platform
  || !Array.isArray(manifest.capabilities)
) {
  throw new Error('Plugin manifest is missing valid update metadata.')
}
if (tag !== manifest.version && tag !== 'v' + manifest.version) {
  throw new Error('Release tag must match the plugin manifest version.')
}
if (
  manifest.platform.os !== 'windows'
  || !['x86_64', 'aarch64'].includes(manifest.platform.architecture)
  || manifest.platform.abi !== 'msvc'
) {
  throw new Error('The release manifest must target Windows MSVC.')
}

const packageName = manifest.id + '-' + manifest.version
  + '-windows-' + manifest.platform.architecture + '.wplug'
if (basename(packagePath) !== packageName) {
  throw new Error('Release package must be named ' + packageName + '.')
}

const packageBytes = readFileSync(packagePath)
if (packageBytes.length < 1 || packageBytes.length > 100 * 1024 * 1024) {
  throw new Error('The .wplug archive must be between 1 byte and 100 MiB.')
}

const payload = {
  schemaVersion: 1,
  id: manifest.id,
  version: manifest.version,
  releaseNotesUrl: 'https://github.com/' + repository + '/releases/tag/' + tag,
  downloadUrl: 'https://github.com/' + repository + '/releases/download/' + tag + '/' + packageName,
  sha256: createHash('sha256').update(packageBytes).digest('hex'),
  sizeBytes: packageBytes.length,
  hostCompatibility: manifest.hostCompatibility,
  uiBridgeCompatibility: manifest.ui?.bridgeCompatibility ?? null,
  platform: manifest.platform,
  capabilities: manifest.capabilities,
  networkPublicHosts: manifest.networkPublicHosts ?? [],
  provides: manifest.provides ?? [],
  requires: manifest.requires ?? [],
}

const privateKeyDer = Buffer.concat([
  Buffer.from('302e020100300506032b657004220420', 'hex'),
  seed,
])
const privateKey = createPrivateKey({
  key: privateKeyDer,
  format: 'der',
  type: 'pkcs8',
})
const payloadText = JSON.stringify(payload)
const signature = sign(null, Buffer.from(payloadText, 'utf8'), privateKey)
const publicKey = createPublicKey(privateKey)
const derivedPublicKeyHex = publicKey.export({ format: 'der', type: 'spki' }).subarray(-32).toString('hex')
const expectedPublicKeyHex = (process.env.PLUGIN_UPDATE_SIGNING_PUBLIC_KEY ?? '').trim()
if (!/^[a-f0-9]{64}$/.test(expectedPublicKeyHex)) {
  throw new Error('PLUGIN_UPDATE_SIGNING_PUBLIC_KEY must be the registered 32-byte lowercase hex public key.')
}
if (derivedPublicKeyHex !== expectedPublicKeyHex) {
  throw new Error('PLUGIN_UPDATE_SIGNING_KEY does not match PLUGIN_UPDATE_SIGNING_PUBLIC_KEY.')
}
if (!verify(null, Buffer.from(payloadText, 'utf8'), publicKey, signature)) {
  throw new Error('Generated update signature could not be verified.')
}

const envelope = {
  schemaVersion: 1,
  payload: payloadText,
  signature: signature.toString('hex'),
}
mkdirSync(dirname(outputPath), { recursive: true })
writeFileSync(outputPath, JSON.stringify(envelope, null, 2) + '\n', {
  encoding: 'utf8',
  flag: 'wx',
})
console.log('Created signed update manifest for ' + manifest.id + ' v' + manifest.version + '.')
