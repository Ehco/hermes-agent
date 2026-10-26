import { statSync } from 'node:fs'
import path from 'node:path'

import { buildDesktopBackendEnv } from './backend-env'

/**
 * HERMES_AGENT_BIN — a supported override that makes Desktop launch the Rust
 * gateway binary (the Rust port's `hermes-gateway`, crates/gateway) instead of
 * the Python pipeline. Unset — or pointing at a path that is not a readable
 * file — keeps the default Python resolution ladder below completely
 * untouched, so this is zero-risk for existing installs.
 *
 * The binary is spawned through the SAME bounded client protocol the Python
 * CLI serves (local-gateway.ts `runGatewayEnsure`): Desktop runs
 * `<bin> [--profile P] gateway ensure --json`, the child ensures a detached
 * gateway owner and prints the single-line JSON verdict
 * `{state:'ready', endpoint:{profile_id, instance_id, runtime_protocol:1,
 * api_origin, capabilities:['session-authority-v1'], ...}}` whose
 * `api_origin` is the loopback REST + `/api/ws` origin the UI dials. Nothing
 * else in the boot changes: no ready-file, no fixed port, no stdout parsing
 * beyond the ensure verdict.
 *
 * Ticket grants differ from Python only in where they are minted: the Rust
 * owner serves no Python control pipe/socket for the bridge to answer, so
 * Desktop mints over the gateway's own `POST /api/auth/ws-ticket` (the same
 * endpoint the Python gateway exposes). See local-gateway.ts
 * `configureRestGatewayTicketClient`.
 */
export interface AgentBinBackend {
  kind: 'command'
  label: string
  command: string
  args: string[]
  env: NodeJS.ProcessEnv
  root: string
  bootstrap: false
  shell: false
  local: 'bundled'
  /** Marks this backend as the Rust agent: ticket minting goes over the gateway's own REST mint. */
  rustAgent: true
}

/**
 * Resolve HERMES_AGENT_BIN into a spawnable backend, or null when the
 * override is unset/blank/not-a-file (the caller then falls through to the
 * Python ladder and logs why).
 *
 * `isFile` is injectable for tests; `statSync(p).isFile()` in production.
 * Relative paths resolve against the Desktop process cwd, like every other
 * resolver here.
 */
export function resolveAgentBinBackend(
  raw: string | undefined,
  backendArgs: string[],
  { currentEnv, isFile }: {
    currentEnv?: NodeJS.ProcessEnv
    isFile?: (candidate: string) => boolean
  } = {}
): AgentBinBackend | null {
  const value = String(raw ?? '').trim()

  if (!value) {
    return null
  }

  const resolved = path.resolve(value)
  const probe = isFile ?? defaultIsFile

  if (!probe(resolved)) {
    return null
  }

  return {
    kind: 'command',
    label: `Rust agent (HERMES_AGENT_BIN) at ${resolved}`,
    command: resolved,
    args: [...backendArgs],
    // Same scrubbed env the Python rungs hand the spawn (PYTHONPATH/HOME
    // leakage removed); HERMES_HOME and the profile parent env ride with the
    // spawn site in main.ts / runGatewayEnsure.
    env: buildDesktopBackendEnv({ currentEnv: currentEnv ?? process.env }),
    root: path.dirname(resolved),
    bootstrap: false,
    shell: false,
    local: 'bundled',
    rustAgent: true
  }
}

function defaultIsFile(candidate: string): boolean {
  try {
    return statSync(candidate).isFile()
  } catch {
    return false
  }
}
