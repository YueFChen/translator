#!/usr/bin/env node

import { generateKeyPairSync } from 'node:crypto'
import { mkdirSync, writeFileSync, existsSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import path from 'node:path'

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const secretDirectory = path.join(root, '.secrets')
const privateKeyPath = path.join(secretDirectory, 'PLUGIN_UPDATE_SIGNING_KEY')
const publicKeyPath = path.join(secretDirectory, 'translator-signing-public-key.txt')

if (existsSync(privateKeyPath) || existsSync(publicKeyPath)) {
  throw new Error('Signing key files already exist. Refusing to replace an existing key.')
}

const { publicKey, privateKey } = generateKeyPairSync('ed25519')
const publicDer = publicKey.export({ format: 'der', type: 'spki' })
const privateDer = privateKey.export({ format: 'der', type: 'pkcs8' })
const publicKeyHex = publicDer.subarray(-32).toString('hex')
const privateSeedBase64 = privateDer.subarray(-32).toString('base64')

mkdirSync(secretDirectory, { recursive: true })
writeFileSync(privateKeyPath, privateSeedBase64, {
  encoding: 'utf8',
  flag: 'wx',
  mode: 0o600,
})
writeFileSync(publicKeyPath, publicKeyHex + '\n', {
  encoding: 'utf8',
  flag: 'wx',
})

console.log('Created a local Ed25519 signing key pair in .secrets/.')
console.log('Private seed: .secrets/PLUGIN_UPDATE_SIGNING_KEY (never commit or print this file).')
console.log('Public key: .secrets/translator-signing-public-key.txt')
