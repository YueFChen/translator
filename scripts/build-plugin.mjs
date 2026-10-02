import './check-core-compatibility.mjs'
import { createHash } from 'node:crypto'
import { cp, mkdir, readdir, readFile, rename, rm, writeFile } from 'node:fs/promises'
import { spawnSync } from 'node:child_process'
import { fileURLToPath } from 'node:url'
import path from 'node:path'

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const release = process.argv.includes('--release')
const archive = process.argv.includes('--archive')
const manifest = JSON.parse(await readFile(path.join(root, 'package/manifest.json'), 'utf8'))
const packageJson = JSON.parse(await readFile(path.join(root, 'package.json'), 'utf8'))
const uiPackageJson = JSON.parse(await readFile(path.join(root, 'ui/package.json'), 'utf8'))
const cargoToml = await readFile(path.join(root, 'Cargo.toml'), 'utf8')
const cargoPackageName = cargoToml.match(/^name\s*=\s*"([a-z0-9_-]+)"/m)?.[1]
const workspaceVersion = cargoToml.match(/^\[workspace\.package\][\s\S]*?^version\s*=\s*"([^"]+)"/m)?.[1]
if (!cargoPackageName) throw new Error('Cannot read the package name from Cargo.toml.')
if (!/^[a-z][a-z0-9_-]{0,63}$/.test(manifest.id ?? '')) throw new Error('manifest.json has an invalid plugin ID.')
if (!/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(-[0-9A-Za-z.-]+)?$/.test(manifest.version ?? '')) {
  throw new Error('manifest.json has an invalid plugin version.')
}
if ([packageJson.version, uiPackageJson.version, workspaceVersion].some((version) => version !== manifest.version)) {
  throw new Error('Manifest, root package, UI package, and Rust workspace versions must match.')
}
if (process.env.GITHUB_REF_TYPE === 'tag' && process.env.GITHUB_REF_NAME !== 'v' + manifest.version) {
  throw new Error('Release tag must be v' + manifest.version + '.')
}
if (archive && !release) throw new Error('A .wplug archive can only be created from a release build.')

if (process.platform !== 'win32' || process.arch !== 'x64') {
  throw new Error('This plugin build supports Windows x86_64 MSVC only.')
}
if (
  manifest.platform?.os !== 'windows'
  || manifest.platform?.architecture !== 'x86_64'
  || manifest.platform?.abi !== 'msvc'
) {
  throw new Error('manifest.json does not target Windows x86_64 MSVC.')
}

const targetRoot = path.resolve(root, 'target')
const outputName = manifest.id.replaceAll('_', '-')
const output = path.resolve(targetRoot, release ? `${outputName}-release-package` : `${outputName}-plugin`)
const relativeOutput = path.relative(targetRoot, output)
if (!relativeOutput || relativeOutput.startsWith('..') || path.isAbsolute(relativeOutput)) {
  throw new Error('Refusing to replace an output directory outside this repository target directory.')
}

function run(command, args, cwd = root) {
  let commandName = command
  let commandArgs = args
  if (process.platform === 'win32' && command === 'pnpm') {
    commandName = process.env.ComSpec ?? 'cmd.exe'
    commandArgs = ['/d', '/s', '/c', ['pnpm.cmd', ...args].join(' ')]
  }
  const result = spawnSync(commandName, commandArgs, { cwd, stdio: 'inherit' })
  if (result.error) throw result.error
  if (result.status !== 0) process.exit(result.status ?? 1)
}

run('pnpm', ['--dir', 'ui', 'run', 'typecheck'])
run('pnpm', ['--dir', 'ui', 'run', 'build'])
run('cargo', [
  'build',
  '--locked',
  '--package',
  cargoPackageName,
  ...(release ? ['--release'] : []),
])

await rm(output, { recursive: true, force: true })
await mkdir(output, { recursive: true })
await cp(path.join(root, 'package/manifest.json'), path.join(output, 'manifest.json'))
await cp(path.join(root, 'package/contract.json'), path.join(output, 'contract.json'))

const uiEntry = manifest.ui?.entry
if (!uiEntry?.startsWith('ui/')) throw new Error('manifest.ui.entry must be inside ui/.')
await cp(path.join(root, 'ui/dist'), path.join(output, 'ui'), { recursive: true })

const backendEntry = manifest.backend?.entry
if (!backendEntry?.startsWith('backend/')) throw new Error('manifest.backend.entry must be inside backend/.')
const profile = release ? 'release' : 'debug'
const backendSource = path.join(targetRoot, profile, `${cargoPackageName}.exe`)
const backendDestination = path.join(output, ...backendEntry.split('/'))
await mkdir(path.dirname(backendDestination), { recursive: true })
await cp(backendSource, backendDestination)

const files = await listFiles(output)
const checksums = {
  algorithm: 'sha256',
  files: await Promise.all(files.map(async (file) => ({
    path: file,
    sha256: createHash('sha256').update(await readFile(path.join(output, ...file.split('/')))).digest('hex'),
  }))),
}
await writeFile(path.join(output, 'checksums.json'), `${JSON.stringify(checksums, null, 2)}\n`)

if (archive) {
  const archivePath = path.join(
    targetRoot,
    `${manifest.id}-${manifest.version}-windows-x86_64.wplug`,
  )
  const zipPath = `${archivePath}.zip`
  await rm(archivePath, { force: true })
  await rm(zipPath, { force: true })
  const source = quotePowerShell(path.join(output, '*'))
  const destination = quotePowerShell(zipPath)
  run('powershell.exe', [
    '-NoProfile',
    '-NonInteractive',
    '-Command',
    `Compress-Archive -Path ${source} -DestinationPath ${destination} -CompressionLevel Optimal`,
  ])
  await rename(zipPath, archivePath)
  console.log(`Release package created: ${archivePath}`)
} else {
  console.log(`Plugin package created: ${output}`)
}

async function listFiles(directory, prefix = '') {
  const entries = await readdir(directory, { withFileTypes: true })
  const files = []
  for (const entry of entries) {
    const relative = prefix ? `${prefix}/${entry.name}` : entry.name
    if (entry.isDirectory()) files.push(...await listFiles(path.join(directory, entry.name), relative))
    else if (entry.isFile()) files.push(relative)
    else throw new Error(`Unsupported package entry: ${relative}`)
  }
  return files.sort()
}

function quotePowerShell(value) {
  return `'${value.replaceAll("'", "''")}'`
}
