import assert from 'node:assert/strict'
import os from 'node:os'
import path from 'node:path'

import { test } from 'vitest'

import { resolveAgentBinBackend } from './agent-bin'

const ARGS = ['--profile', 'work', 'gateway', 'ensure', '--json']

test('unset / blank HERMES_AGENT_BIN resolves to null (Python default untouched)', () => {
  assert.equal(resolveAgentBinBackend(undefined, ARGS), null)
  assert.equal(resolveAgentBinBackend('', ARGS), null)
  assert.equal(resolveAgentBinBackend('   ', ARGS), null)
})

test('an unresolvable override name falls through like a missing file', () => {
  // No isFile injection: the default probe answers for real files only, so a
  // PATH-style bare name (no existing file behind it in cwd) resolves to null.
  assert.equal(resolveAgentBinBackend('definitely-not-here.exe', ARGS), null)
})

test('a directory is not a backend', () => {
  assert.equal(resolveAgentBinBackend(os.tmpdir(), ARGS), null)
})

test('an existing file resolves the Rust agent backend contract', () => {
  const env = { PYTHONHOME: '/leaked/python', PYTHONPATH: '/leaked/other' }
  const file = path.join('C:\\tools', 'hermes-gateway.exe')
  const backend = resolveAgentBinBackend(file, ARGS, {
    currentEnv: env,
    isFile: candidate => candidate === file
  })

  assert.ok(backend)
  assert.equal(backend.command, file)
  assert.equal(backend.shell, false)
  assert.equal(backend.bootstrap, false)
  assert.equal(backend.kind, 'command')
  assert.equal(backend.rustAgent, true)
  assert.deepEqual(backend.args, ARGS)
  assert.equal(backend.root, 'C:\\tools')
  assert.equal(backend.env.PYTHONHOME, '')
  assert.equal(backend.env.PYTHONPATH, '')
})

test('relative overrides resolve against the process cwd', () => {
  // Injectable probe: the value need not exist on disk for this assertion.
  const backend = resolveAgentBinBackend(path.join('bin', 'hermes-gateway.exe'), [], { isFile: () => true })

  assert.ok(backend)
  assert.equal(backend.command, path.resolve(process.cwd(), 'bin', 'hermes-gateway.exe'))
  assert.equal(path.isAbsolute(backend.command), true)
})
