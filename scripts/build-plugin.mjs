import { cp, mkdir, rm } from 'node:fs/promises'
import { spawnSync } from 'node:child_process'
import { fileURLToPath } from 'node:url'
import path from 'node:path'

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
if (process.platform !== 'win32' || process.arch !== 'x64') throw new Error('The translator package currently targets Windows x86_64 MSVC.')
function run(command, args) {
  const result = spawnSync(command, args, { cwd: root, stdio: 'inherit', shell: process.platform === 'win32' })
  if (result.status !== 0) process.exit(result.status ?? 1)
}
run('pnpm', ['--dir', 'ui', 'run', 'build'])
run('cargo', ['build', '--locked', '--package', 'wonderland-translator', '--bin', 'wonderland-translator'])

const targetRoot = path.resolve(root, 'target')
const output = path.resolve(targetRoot, 'translator-plugin')
const relativeOutput = path.relative(targetRoot, output)
if (!relativeOutput || relativeOutput.startsWith('..') || path.isAbsolute(relativeOutput)) throw new Error('Refusing to replace a plugin output outside this repository target directory.')
const packageRoot = path.join(root, 'package')
await rm(output, { recursive: true, force: true })
await mkdir(path.join(output, 'backend'), { recursive: true })
await cp(path.join(packageRoot, 'manifest.json'), path.join(output, 'manifest.json'))
await cp(path.join(packageRoot, 'contract.json'), path.join(output, 'contract.json'))
await cp(path.join(root, 'ui', 'dist'), path.join(output, 'ui'), { recursive: true })
await cp(path.join(targetRoot, 'debug', 'wonderland-translator.exe'), path.join(output, 'backend', 'wonderland-translator.exe'))
console.log('Debug plugin package created.')
