import { readFile } from 'node:fs/promises'
import { fileURLToPath } from 'node:url'
import path from 'node:path'

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const core = path.resolve(root, '../..')
const manifest = JSON.parse(await readFile(path.join(root, 'package/manifest.json'), 'utf8'))
const cargo = await readFile(path.join(core, 'Cargo.toml'), 'utf8')
const coreVersion = cargo.match(/^\[workspace\.package\][\s\S]*?^version\s*=\s*"([^"\n]+)"/m)?.[1]
const tuple = (value) => {
  if (!/^\d+\.\d+\.\d+$/.test(value ?? '')) throw new Error(`Expected a stable Core version, got ${value}`)
  return value.split('.').map(Number)
}
const compare = (a, b) => {
  const left = tuple(a); const right = tuple(b)
  for (let i = 0; i < 3; i++) if (left[i] !== right[i]) return left[i] - right[i]
  return 0
}
const { minCoreVersion, maxCoreVersionExclusive } = manifest.hostCompatibility
if (compare(coreVersion, minCoreVersion) < 0 || compare(coreVersion, maxCoreVersionExclusive) >= 0) {
  throw new Error(`This plugin requires Core >= ${minCoreVersion} and < ${maxCoreVersionExclusive}; checkout is ${coreVersion}.`)
}
if (manifest.remoteAccess === true && manifest.backend.supportsServiceContext !== true) {
  throw new Error('Remote access requires backend.supportsServiceContext = true.')
}
console.log(`Core ${coreVersion}; remote access ${manifest.remoteAccess === true ? 'enabled' : 'not declared (local only)'}.`)
