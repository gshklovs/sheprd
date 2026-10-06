import { expect, mock, test } from 'claude-code/testing'
import type { On } from 'claude-code'
import type { MockClock } from 'claude-code/testing'
import { findDevUrls } from '../hooks/register.ts'

type Posted = { url: string; body: Record<string, unknown> }
type World = Posted[] & { clock: MockClock; polls: string[] }
type WorldOpts = {
  isDown?: boolean
  isToolRegisterOwn?: boolean
  // answers GET /api/mod/inbox polls in turn (then empty); `null` = the server is down for that poll
  inbox?: (unknown[] | null)[]
  // answers other GETs by URL prefix
  gets?: Record<string, (url: string) => unknown>
  // answers POSTs by URL part (the body is still recorded)
  posts?: Record<string, (body: Record<string, unknown>) => unknown>
}

// The world beneath the plugin: env, clock, session, tool registry and the network.
function world(on: On, opts: WorldOpts = {}): World {
  const posted = [] as unknown as World
  mock.env(on, { HERDR_PANE_ID: 'w6:p1', HOME: '/home/u' })
  posted.clock = mock.clock(on)
  posted.polls = []
  mock.store(on)
  on('session.id', () => ({ value: 'sess-123' }))
  on('session.cwd', () => ({ value: '/work/app' }))
  if (!opts.isToolRegisterOwn) on('tool.register', ($, e) => ({ value: { tool: `mcp__sheprd__${e.name}` } }))
  on('session.start', ($, e) => ({ cwd: e.cwd }))
  on('http.fetch', ($, e) => {
    // never a throw: a test hook that throws is skipped and the real network would answer
    if (opts.isDown) return { value: { status: 503, ok: false, headers: {}, text: 'down' } }
    if (e.url.includes('/api/mod/inbox?')) {
      posted.polls.push(e.url + ' @' + posted.clock.now())
      const next = opts.inbox?.shift()
      if (next === null) return { value: { status: 502, ok: false, headers: {}, text: 'down' } }
      return { value: { status: 200, ok: true, headers: {}, text: JSON.stringify({ seq: 1, messages: next ?? [] }) } }
    }
    const get = Object.entries(opts.gets ?? {}).find(([k]) => e.url.includes(k))
    if (get && !e.init?.body) {
      return { value: { status: 200, ok: true, headers: {}, text: JSON.stringify(get[1](e.url)) } }
    }
    const body = JSON.parse(e.init?.body ?? '{}') as Record<string, unknown>
    posted.push({ url: e.url, body })
    const post = Object.entries(opts.posts ?? {}).find(([k]) => e.url.includes(k))
    if (post) return { value: { status: 200, ok: true, headers: {}, text: JSON.stringify(post[1](body)) } }
    return { value: { status: 200, ok: true, headers: {}, text: '{"ok":true,"id":"s1"}' } }
  })
  return posted
}

const START = { cwd: '/work/app', surface: 'terminal', isInteractive: true } as const

test('surface tool POSTs the item with session and pane', async ($, on) => {
  const posted = world(on)
  const ran = await $.tool.call({
    tool: 'mcp__sheprd__surface',
    title: 'Dev server is up',
    body: 'Vite on 5173',
    level: 'info',
    images: ['shots/home.png', '/tmp/abs.png'],
    links: [{ url: 'http://localhost:5173', label: 'app' }],
  })
  expect(posted.length).toBe(1)
  expect(posted[0]?.url).toBe('http://127.0.0.1:7878/api/surface')
  expect(posted[0]?.body).toEqual({
    title: 'Dev server is up',
    level: 'info',
    body: 'Vite on 5173',
    images: ['/work/app/shots/home.png', '/tmp/abs.png'],
    links: [{ url: 'http://localhost:5173', label: 'app' }],
    tool: true,
    session: 'sess-123',
    pane: 'w6:p1',
  })
  expect(ran.deny).toBeUndefined()
  expect(String(ran.result)).toContain('Surfaced')
})

test('base URL comes from userConfig', { options: { baseUrl: 'http://127.0.0.1:7899/' } }, async ($, on) => {
  const posted = world(on)
  await $.tool.call({ tool: 'mcp__sheprd__surface', title: 'Done', level: 'done' })
  expect(posted[0]?.url).toBe('http://127.0.0.1:7899/api/surface')
})

test('surface tool fails soft when the server is down', async ($, on) => {
  world(on, { isDown: true })
  const ran = await $.tool.call({ tool: 'mcp__sheprd__surface', title: 'Need a decision', level: 'attention' })
  expect(ran.deny).toBeUndefined()
  expect(ran.isError).toBeUndefined()
  expect(String(ran.result)).toContain('Noted')
})

test('session.start registers the tool', async ($, on) => {
  world(on, { isToolRegisterOwn: true })
  const names: string[] = []
  on('tool.register', ($, e) => {
    names.push(e.name)
    return { value: { tool: `mcp__sheprd__${e.name}` } }
  })
  await $.session.start({ cwd: '/work/app', surface: 'terminal', isInteractive: true })
  expect(names).toEqual(['surface'])
})

test('prompt.compose adds a short section only where the tool is offered', async ($, on) => {
  world(on)
  on('prompt.compose', () => ({ sections: [{ id: 'intro', text: 'You are Claude.', scope: 'shared' as const }] }))
  const base = { model: 'm', promptModel: 'm', surfaces: [], outputStyle: null, traits: [] }
  const withTool = await $.prompt.compose({ ...base, tools: ['Bash', 'mcp__sheprd__surface'] })
  const added = withTool.sections.find(s => s.id === 'sheprd:phone')
  expect(added?.scope).toBe('session')
  expect((added?.text ?? '').split(/\s+/).length).toBeLessThan(141)
  const without = await $.prompt.compose({ ...base, tools: ['Bash'] })
  expect(without.sections.length).toBe(1)
})

test('the home brief sends resource questions to /api/res/brief; the worker guide does not', async ($, on) => {
  world(on, { gets: { '/api/home': () => ({ pane: 'w6:p1', session: 'sess-123' }) } })
  on('prompt.compose', () => ({ sections: [{ id: 'intro', text: 'You are Claude.', scope: 'shared' as const }] }))
  const base = { model: 'm', promptModel: 'm', surfaces: [], outputStyle: null, traits: [] }
  const out = await $.prompt.compose({ ...base, tools: ['Bash', 'mcp__sheprd__surface'] })
  const home = out.sections.find(s => s.id === 'sheprd:home')
  expect(home?.text ?? '').toContain('/api/res/brief')
  const phone = out.sections.find(s => s.id === 'sheprd:phone')
  expect(phone?.text ?? 'missing').not.toContain('/api/res')
})

test('forwards a You should know note once', {
  plugins: [
    {
      name: 'cc-plugin-you-should-know',
      tier: 'builtin',
      register(on) {
        on('session.start', async ($, e, next) => {
          const view = { kind: 'offer', line: 'The migration drops a column', explanation: 'users.email is still read by the API', shownAt: Date.now() }
          await $.store.set('sessions', [{ id: 'sess-123', view }])
          await $.store.set('sessions', [{ id: 'sess-123', view, tag: 'x' }])
          await $.store.set('sessions', [{ id: 'sess-123', view: { kind: 'asked', shownAt: Date.now() } }])
          return next(e)
        })
      },
    },
  ],
}, async ($, on) => {
  const posted = world(on)
  await $.session.start({ cwd: '/work/app', surface: 'terminal', isInteractive: true })
  expect(posted.map(p => p.body)).toEqual([
    {
      title: 'The migration drops a column',
      level: 'attention',
      body: '**You should know** - users.email is still read by the API',
      tool: false,
      session: 'sess-123',
      pane: 'w6:p1',
    },
  ])
})

test('ignores another plugin writing a "sessions" key', {
  plugins: [
    {
      name: 'someone-else',
      register(on) {
        on('session.start', async ($, e, next) => {
          await $.store.set('sessions', [{ id: 'x', view: { kind: 'offer', line: 'nope', shownAt: Date.now() } }])
          return next(e)
        })
      },
    },
  ],
}, async ($, on) => {
  const posted = world(on)
  await $.session.start({ cwd: '/work/app', surface: 'terminal', isInteractive: true })
  expect(posted.length).toBe(0)
})

test('auto-surfaces a PNG a Bash command just wrote, once per minute', async ($, on) => {
  const posted = world(on)
  on('fs.stat', ($, e) => ({ value: { kind: 'file', size: 10, mtimeMs: Date.now(), isLink: false } }))
  on('tool.call', { tool: 'Bash' }, () => ({ result: { stdout: '', stderr: '', interrupted: false } }))
  await $.tool.call({ tool: 'Bash', command: 'grim -g "$(slurp)" ./shot.png' })
  await $.tool.call({ tool: 'Bash', command: 'grim /tmp/other.png' })
  expect(posted.map(p => p.body.images)).toEqual([['/work/app/shot.png']])
  expect(posted[0]?.body.level).toBe('info')
})

test('does not auto-surface an old PNG a command merely mentions', async ($, on) => {
  const posted = world(on)
  on('fs.stat', () => ({ value: { kind: 'file', size: 10, mtimeMs: 1000, isLink: false } }))
  on('tool.call', { tool: 'Bash' }, () => ({ result: { stdout: '', stderr: '', interrupted: false } }))
  await $.tool.call({ tool: 'Bash', command: 'ls -la /tmp/old.png' })
  expect(posted.length).toBe(0)
})

test('auto-surface can be turned off', { options: { autoSurface: false } }, async ($, on) => {
  const posted = world(on)
  on('fs.stat', () => ({ value: { kind: 'file', size: 10, mtimeMs: Date.now(), isLink: false } }))
  on('tool.call', { tool: 'Bash' }, () => ({ result: { stdout: '', stderr: '', interrupted: false } }))
  await $.tool.call({ tool: 'Bash', command: 'grim /tmp/new.png' })
  expect(posted.length).toBe(0)
})

test('auto-surfaces an image an MCP screenshot tool returned', async ($, on) => {
  const posted = world(on)
  const runs: string[][] = []
  on('process.run', ($, e) => {
    runs.push([...e.argv])
    return { value: { exitCode: 0, stdout: '', stderr: '', isStdoutTruncated: false, isStderrTruncated: false } }
  })
  on('tool.call', { tool: 'mcp__playwright__browser_take_screenshot' }, () => ({
    result: [{ type: 'image', data: 'A'.repeat(200), mimeType: 'image/png' }],
  }))
  await $.tool.call({ tool: 'mcp__playwright__browser_take_screenshot' })
  expect(runs.length).toBe(1)
  expect(posted.length).toBe(1)
  const images = posted[0]?.body.images as string[]
  expect(images[0]?.startsWith('/home/u/.cache/sheprd/shots/')).toBe(true)
})

test('surface tool forwards replies and command (trimmed, capped)', async ($, on) => {
  const posted = world(on)
  const ran = await $.tool.call({
    tool: 'mcp__sheprd__surface',
    title: 'Run this sudo command',
    level: 'attention',
    command: '  sudo pacman -S mosh  ',
    replies: ['done', ' its up now ', '', 'a', 'b', 'c'],
  })
  expect(posted[0]?.body).toEqual({
    title: 'Run this sudo command',
    level: 'attention',
    command: 'sudo pacman -S mosh',
    replies: ['done', 'its up now', 'a', 'b'],
    tool: true,
    session: 'sess-123',
    pane: 'w6:p1',
  })
  expect(String(ran.result)).toContain('end your turn')
})

test('surface tool omits empty replies and command', async ($, on) => {
  const posted = world(on)
  await $.tool.call({ tool: 'mcp__sheprd__surface', title: 'x', level: 'info', replies: [], command: ' ' })
  expect('replies' in (posted[0]?.body ?? {})).toBe(false)
  expect('command' in (posted[0]?.body ?? {})).toBe(false)
})

test('surface schema offers replies and command; guide mentions hand-offs', async ($, on) => {
  world(on, { isToolRegisterOwn: true })
  const schemas: Record<string, unknown>[] = []
  on('tool.register', ($, e) => {
    schemas.push(e.inputSchema as Record<string, unknown>)
    return { value: { tool: `mcp__sheprd__${e.name}` } }
  })
  on('prompt.compose', () => ({ sections: [] }))
  await $.session.start({ cwd: '/work/app', surface: 'terminal', isInteractive: true })
  const props = (schemas[0]?.properties ?? {}) as Record<string, { type?: string; maxItems?: number }>
  expect(props.replies?.type).toBe('array')
  expect(props.replies?.maxItems).toBe(4)
  expect(props.command?.type).toBe('string')
  const base = { model: 'm', promptModel: 'm', surfaces: [], outputStyle: null, traits: [] }
  const composed = await $.prompt.compose({ ...base, tools: ['mcp__sheprd__surface'] })
  expect(composed.sections.find(s => s.id === 'sheprd:phone')?.text).toContain('replies: ["done"]')
})

const QUESTIONS = [
  {
    header: 'Beverage',
    question: 'Do you prefer tea or coffee?',
    multiSelect: false,
    options: [
      { label: 'Tea', description: 'A cup of tea' },
      { label: 'Coffee', description: 'A cup of coffee' },
    ],
  },
]

test('AskUserQuestion posts an ask hint before the dialog and ask_clear after', async ($, on) => {
  const posted = world(on)
  let seenWhileOpen = 0
  on('tool.call', { tool: 'AskUserQuestion' }, () => {
    seenWhileOpen = posted.length
    return { result: { questions: QUESTIONS, answers: { 'Do you prefer tea or coffee?': 'Tea' } } }
  })
  const ran = await $.tool.call({ tool: 'AskUserQuestion', questions: QUESTIONS })
  expect(seenWhileOpen).toBe(1)
  expect(posted.map(p => p.url)).toEqual(['http://127.0.0.1:7878/api/hint', 'http://127.0.0.1:7878/api/hint'])
  expect(posted[0]?.body).toEqual({ kind: 'ask', questions: QUESTIONS, session: 'sess-123', pane: 'w6:p1' })
  expect(posted[1]?.body).toEqual({ kind: 'ask_clear', session: 'sess-123', pane: 'w6:p1' })
  expect(ran.isError).toBeUndefined()
})

test('AskUserQuestion still clears when the dialog errors', async ($, on) => {
  const posted = world(on)
  on('tool.call', { tool: 'AskUserQuestion' }, () => {
    throw new Error('dismissed')
  })
  await $.tool.call({ tool: 'AskUserQuestion', questions: QUESTIONS }).catch(() => undefined)
  expect(posted.map(p => p.body.kind)).toEqual(['ask', 'ask_clear'])
})

test('AskUserQuestion is unaffected when the server is down', async ($, on) => {
  world(on, { isDown: true })
  on('tool.call', { tool: 'AskUserQuestion' }, () => ({ result: { questions: QUESTIONS, answers: {} } }))
  const ran = await $.tool.call({ tool: 'AskUserQuestion', questions: QUESTIONS })
  expect(ran.isError).toBeUndefined()
})

test('ExitPlanMode posts the plan before the approval menu and plan_clear after', async ($, on) => {
  const posted = world(on)
  let seenWhileOpen = 0
  on('tool.call', { tool: 'ExitPlanMode' }, () => {
    seenWhileOpen = posted.length
    return { result: { plan: '# Plan', isAgent: false } }
  })
  await $.tool.call({ tool: 'ExitPlanMode', plan: '# Plan\n- a', planFilePath: '/p/plan.md' } as never)
  expect(seenWhileOpen).toBe(1)
  expect(posted[0]?.body).toEqual({ kind: 'plan', plan: '# Plan\n- a', path: '/p/plan.md', session: 'sess-123', pane: 'w6:p1' })
  expect(posted[1]?.body).toEqual({ kind: 'plan_clear', session: 'sess-123', pane: 'w6:p1' })
})

test('ExitPlanMode reads the plan file when the input has no plan', async ($, on) => {
  const posted = world(on)
  on('fs.read', () => ({ value: '# From file' }))
  on('tool.call', { tool: 'ExitPlanMode' }, () => ({ result: { plan: null, isAgent: false } }))
  await $.tool.call({ tool: 'ExitPlanMode', planFilePath: '/p/plan.md' } as never)
  expect(posted.map(p => p.body.kind)).toEqual(['plan', 'plan_clear'])
  expect(posted[0]?.body.plan).toBe('# From file')
})

test('ExitPlanMode without any plan posts nothing', async ($, on) => {
  const posted = world(on)
  on('tool.call', { tool: 'ExitPlanMode' }, () => ({ result: { plan: null, isAgent: false } }))
  await $.tool.call({ tool: 'ExitPlanMode' } as never)
  expect(posted.length).toBe(0)
})

test('findDevUrls: hosts, ports, dedupe, normalisation', async () => {
  expect(findDevUrls('  VITE v5  ready\n  ➜  Local:   http://localhost:5173/\n  ➜  Network: use --host')).toEqual([
    { port: 5173, url: 'http://localhost:5173/' },
  ])
  expect(findDevUrls('Serving HTTP on 0.0.0.0 port 8765 (http://0.0.0.0:8765/) ...')).toEqual([
    { port: 8765, url: 'http://localhost:8765/' },
  ])
  expect(findDevUrls('a http://127.0.0.1:3000/api b https://localhost:8443 c http://127.0.0.1:3000/x')).toEqual([
    { port: 3000, url: 'http://127.0.0.1:3000/' },
    { port: 8443, url: 'https://localhost:8443/' },
  ])
  expect(findDevUrls('http://[::1]:4000')).toEqual([{ port: 4000, url: 'http://localhost:4000/' }])
  expect(findDevUrls('http://example.com:8080 http://localhost/ http://localhost:99999 http://192.168.1.2:3000')).toEqual([])
})

function bashResult(stdout: string, extra: Record<string, unknown> = {}) {
  return { result: { stdout, stderr: '', interrupted: false, ...extra } }
}

test('auto-surfaces a dev server URL from foreground Bash output, once per port', async ($, on) => {
  const posted = world(on)
  let out = ''
  on('tool.call', { tool: 'Bash' }, () => bashResult(out))
  out = 'Serving HTTP on 0.0.0.0 port 8765 (http://0.0.0.0:8765/) ...'
  await $.tool.call({ tool: 'Bash', command: 'timeout 2 python -m http.server 8765' })
  out = 'again http://localhost:8765/'
  await $.tool.call({ tool: 'Bash', command: 'npm run dev' })
  out = '  Local: http://localhost:5173/'
  await $.tool.call({ tool: 'Bash', command: 'npm run dev' })
  expect(posted.map(p => p.body)).toEqual([
    {
      title: 'Dev server :8765',
      level: 'info',
      links: [{ url: 'http://localhost:8765/', label: ':8765' }],
      tool: false,
      session: 'sess-123',
      pane: 'w6:p1',
    },
    {
      title: 'Dev server :5173',
      level: 'info',
      links: [{ url: 'http://localhost:5173/', label: ':5173' }],
      tool: false,
      session: 'sess-123',
      pane: 'w6:p1',
    },
  ])
})

test('dev server URL in the command counts, but not a curl to it or the sheprd port', async ($, on) => {
  const posted = world(on)
  on('tool.call', { tool: 'Bash' }, () => bashResult(''))
  await $.tool.call({ tool: 'Bash', command: 'curl -s http://localhost:3000/api/health' })
  await $.tool.call({ tool: 'Bash', command: 'echo "app at http://127.0.0.1:7878/"' })
  await $.tool.call({ tool: 'Bash', command: 'xdg-open http://localhost:4321' })
  expect(posted.map(p => p.body.title)).toEqual(['Dev server :4321'])
})

test('skips background Bash and auto-backgrounded commands', async ($, on) => {
  const posted = world(on)
  on('tool.call', { tool: 'Bash' }, ($, e) =>
    e.command === 'b' ? bashResult('http://localhost:5000', { backgroundTaskId: 'bx1' }) : bashResult('http://localhost:5001'),
  )
  await $.tool.call({ tool: 'Bash', command: 'a', run_in_background: true })
  await $.tool.call({ tool: 'Bash', command: 'b' })
  expect(posted.length).toBe(0)
})

test('dev server links follow autoSurface', { options: { autoSurface: false } }, async ($, on) => {
  const posted = world(on)
  on('tool.call', { tool: 'Bash' }, () => bashResult('http://localhost:5173/'))
  await $.tool.call({ tool: 'Bash', command: 'npm run dev' })
  expect(posted.length).toBe(0)
})

test('inbox: long-polls with session, pane and since; a steer the server saw as working is held, then submitted when no turn claims it', async ($, on) => {
  const w = world(on, { inbox: [[{ seq: 41, kind: 'steer', text: 'also print PINEAPPLE', working: true }]] })
  const submitted: string[] = []
  on('prompt.submit', ($, e) => {
    submitted.push(e.text)
    return { text: e.text }
  })
  await $.session.start(START)
  await w.clock.advance(60)
  await w.clock.advance(60)
  expect(w.polls[0]).toBe('http://127.0.0.1:7878/api/mod/inbox?session=sess-123&since=0&pane=w6%3Ap1 @50')
  expect(w.polls[1]).toContain('since=41')
  const ack = w.find(p => p.url.endsWith('/api/mod/ack'))?.body
  expect([ack?.seq, ack?.ok, ack?.via, ack?.session, ack?.pane]).toEqual([41, true, 'held', 'sess-123', 'w6:p1'])
  expect(submitted).toEqual([])
  await w.clock.advance(3_000)
  expect(submitted).toEqual(['also print PINEAPPLE'])
})

test('inbox: a steer while idle becomes a prompt of its own', async ($, on) => {
  const w = world(on, { inbox: [[{ seq: 7, kind: 'steer', text: 'next thing', working: false }]] })
  const submitted: string[] = []
  on('prompt.submit', ($, e) => {
    submitted.push(e.text)
    return { text: e.text }
  })
  await $.session.start(START)
  await w.clock.advance(60)
  expect(submitted).toEqual(['next thing'])
  expect(w.find(p => p.url.endsWith('/api/mod/ack'))?.body.via).toBe('submit')
})

test('inbox: backs off while the server is down, then resumes', async ($, on) => {
  const w = world(on, { inbox: [null, null, []] })
  await $.session.start(START)
  await w.clock.advance(60)
  expect(w.polls.length).toBe(1)
  await w.clock.advance(1_900)
  expect(w.polls.length).toBe(1)
  await w.clock.advance(200)
  expect(w.polls.length).toBe(2)
  await w.clock.advance(3_800)
  expect(w.polls.length).toBe(2)
  await w.clock.advance(100)
  expect(w.polls.length).toBe(3)
  await w.clock.advance(60)
  expect(w.polls.length).toBe(4)
})

test('inbox: not started outside a herdr pane', async ($, on) => {
  const polls: string[] = []
  mock.env(on, { HOME: '/home/u' })
  const clock = mock.clock(on)
  on('session.id', () => ({ value: 'sess-123' }))
  on('tool.register', ($, e) => ({ value: { tool: `mcp__sheprd__${e.name}` } }))
  on('session.start', ($, e) => ({ cwd: e.cwd }))
  on('http.fetch', ($, e) => {
    polls.push(e.url)
    return { value: { status: 200, ok: true, headers: {}, text: '{}' } }
  })
  await $.session.start(START)
  await clock.advance(5_000)
  expect(polls.length).toBe(0)
})


const TURN = { turnId: 't1', durationMs: 42_000, isAborted: false }

test('turn.complete posts the exact turn record (main loop only)', async ($, on) => {
  const w = world(on)
  on('turn.complete', ($, e) => ({ text: e.answer }))
  await $.session.start(START)
  await $.turn.complete({ ...TURN, answer: 'All done. PINEAPPLE', reason: 'answer', usage: { input_tokens: 1, output_tokens: 2, cache_read_input_tokens: 3, cache_creation_input_tokens: 4, model: 'm' } })
  await $.turn.complete({ ...TURN, turnId: 't2', answer: 'sub', reason: 'answer', agentId: 'a1' })
  await $.turn.complete({ ...TURN, turnId: 't3', answer: '', reason: 'refusal', refusal: { category: 'cyber', explanation: 'no' } })
  const turns = w.filter(p => p.url.endsWith('/api/mod/turn')).map(p => p.body)
  expect(turns).toEqual([
    { turnId: 't1', answer: 'All done. PINEAPPLE', durationMs: 42_000, reason: 'answer', session: 'sess-123', pane: 'w6:p1',
      usage: { input_tokens: 1, output_tokens: 2, cache_read_input_tokens: 3, cache_creation_input_tokens: 4, model: 'm' } },
    { turnId: 't3', answer: '', durationMs: 42_000, reason: 'refusal', error: 'cyber: no', session: 'sess-123', pane: 'w6:p1' },
  ])
})

test('session.measure posts the gauges', async ($, on) => {
  const w = world(on)
  on('session.measure', ($, e) => ({ changed: e.changed }))
  await $.session.start(START)
  await $.session.measure({
    context: { percent: 63, tokens: 126_000, window: 200_000 },
    rateLimits: [{ kind: 'five_hour', percentUsed: 91.5, resetsAt: '2026-10-05T18:00:00Z' }],
    cost: { usd: 1.84 },
    changed: ['context'],
  })
  expect(w.find(p => p.url.endsWith('/api/mod/measure'))?.body).toEqual({
    context: { percent: 63, tokens: 126_000, window: 200_000 },
    costUsd: 1.84,
    rateLimits: [{ kind: 'five_hour', percentUsed: 91.5, resetsAt: '2026-10-05T18:00:00Z' }],
    session: 'sess-123',
    pane: 'w6:p1',
  })
})

test('inbox: compact runs at once when idle, after the turn when working', async ($, on) => {
  const w = world(on, { inbox: [[{ seq: 5, kind: 'compact', instructions: 'keep the plan' }]] })
  const compacts: unknown[] = []
  on('session.compact', ($, e) => {
    compacts.push(e.instructions)
    return { messages: [] }
  })
  on('session.usage', () => ({ value: { startedAt: 0, context: { window: 200_000, percent: 12, tokens: 24_000 }, rateLimits: [], cost: { usd: 2 } } }))
  await $.session.start(START)
  await w.clock.advance(60)
  expect(compacts).toEqual(['keep the plan'])
  expect(w.find(p => p.url.endsWith('/api/mod/ack'))?.body.via).toBe('compact')
  expect(w.find(p => p.url.endsWith('/api/mod/measure'))?.body.context).toEqual({ percent: 12, tokens: 24_000, window: 200_000 })
})

test('inbox: compact while a turn runs waits for turn.complete', async ($, on) => {
  const w = world(on, { inbox: [[], [{ seq: 6, kind: 'compact' }]] })
  const compacts: unknown[] = []
  on('session.compact', ($, e) => {
    compacts.push(e.instructions ?? null)
    return { messages: [] }
  })
  on('session.usage', () => ({ value: { startedAt: 0, context: { window: 200_000 }, rateLimits: [] } }))
  on('turn.start', ($, e) => ({ turnId: e.turnId }))
  on('turn.complete', ($, e) => ({ text: e.answer }))
  await $.session.start(START)
  await $.turn.start({ text: 'go', turnId: 't1' } as never)
  await w.clock.advance(60)
  await w.clock.advance(60)
  expect(w.find(p => p.url.endsWith('/api/mod/ack'))?.body.via).toBe('deferred')
  expect(compacts).toEqual([])
  await $.turn.complete({ ...TURN, answer: 'ok', reason: 'answer' })
  await w.clock.advance(1_100)
  expect(compacts).toEqual([null])
})

// ---------- talking to agents (server/src/talk.rs) ----------

const USAGE = { input_tokens: 1, output_tokens: 2, cache_read_input_tokens: 0, cache_creation_input_tokens: 0 }

function capture(on: Parameters<typeof world>[0]): string[] {
  const submitted: string[] = []
  on('prompt.submit', ($, e) => {
    submitted.push(e.text)
    return { text: e.text }
  })
  return submitted
}

test('steers during a turn are held, one can be taken back, the rest is appended after the next main-loop tool call', async ($, on) => {
  const w = world(on, {
    inbox: [
      [{ seq: 50, kind: 'steer', text: 'first', working: true }, { seq: 51, kind: 'steer', text: 'second', working: true }],
      [{ seq: 52, kind: 'unsend', ref: 50 }, { seq: 53, kind: 'unsend', ref: 99 }],
    ],
  })
  const submitted = capture(on)
  on('turn.start', ($, e) => ({ turnId: e.turnId }))
  on('tool.call', { tool: 'Read' }, () => ({ result: 'file' }))
  await $.session.start(START)
  await $.turn.start({ text: 'go', turnId: 't1' } as never)
  await w.clock.advance(60)
  const acks = () => w.filter(p => p.url.endsWith('/api/mod/ack')).map(p => [p.body.seq, p.body.ok, p.body.via ?? p.body.error])
  expect(acks()).toEqual([[50, true, 'held'], [51, true, 'held']])
  const held = () => w.filter(p => p.url.endsWith('/api/talk/held')).map(p => (p.body.held as { text: string }[]).map(h => h.text))
  expect(held().at(-1)).toEqual(['first', 'second'])
  expect(submitted).toEqual([])
  await w.clock.advance(60)
  expect(acks().slice(2)).toEqual([[52, true, 'unsent'], [53, false, 'already_sent']])
  expect(held().at(-1)).toEqual(['second'])
  // a subagent's tool call does not flush; the main loop's does (append → falls back to submit in the kit)
  await $.tool.call({ tool: 'Read', file_path: '/a', agentId: 'sub1' } as never)
  await w.clock.advance(10)
  expect(submitted).toEqual([])
  await $.tool.call({ tool: 'Read', file_path: '/a' })
  await w.clock.advance(10)
  expect(held().at(-1)).toEqual([])
  expect(submitted).toEqual(['second']) // the kit refuses plugin appends: the fallback prompt
})

test('steers still held when the turn ends become its next prompt', async ($, on) => {
  const w = world(on, { inbox: [[{ seq: 60, kind: 'steer', text: 'a', working: true }, { seq: 61, kind: 'steer', text: 'b', working: true }]] })
  const submitted = capture(on)
  on('turn.start', ($, e) => ({ turnId: e.turnId }))
  on('turn.complete', ($, e) => ({ text: e.answer }))
  await $.session.start(START)
  await $.turn.start({ text: 'go', turnId: 't1' } as never)
  await w.clock.advance(60)
  await $.turn.complete({ ...TURN, answer: 'ok', reason: 'answer' })
  expect(submitted).toEqual(['a\nb'])
})

test("Claude's own next-prompt suggestion is posted; a plugin's is not", async ($, on) => {
  const w = world(on)
  on('prompt.suggest', () => ({ isShown: true }))
  await $.session.start(START)
  await $.prompt.suggest({ text: '  run the tests  ', origin: { kind: 'suggestion' } } as never)
  const posts = w.filter(p => p.url.endsWith('/api/talk/suggest')).map(p => p.body.text)
  expect(posts).toEqual(['run the tests'])
})

test('a long answer while away gets a TL;DR from a small model; not when someone is watching', async ($, on) => {
  let isAway = true
  const w = world(on, { posts: { '/api/talk/tldr': b => (b.pending ? { ok: true, away: isAway } : { ok: true }) } })
  const models: string[] = []
  on('model.complete', ($, e) => {
    models.push(e.model)
    return { value: { isAnswered: true, text: 'TL;DR: Parser fixed; push needs your OK.', usage: USAGE } }
  })
  on('turn.complete', ($, e) => ({ text: e.answer }))
  await $.session.start(START)
  const long = 'x'.repeat(1600)
  await $.turn.complete({ ...TURN, answer: long, reason: 'answer' })
  await w.clock.advance(10)
  const tl = w.filter(p => p.url.endsWith('/api/talk/tldr')).map(p => p.body)
  expect(tl.map(b => [b.turnId, b.pending ?? null, b.text ?? null])).toEqual([['t1', true, null], ['t1', null, 'Parser fixed; push needs your OK.']])
  expect(models).toEqual(['haiku'])
  // pending is asked before the turn record goes out (so the done push can wait for it)
  const order = w.map(p => p.url.split('/api/')[1]).filter(u => u === 'talk/tldr' || u === 'mod/turn')
  expect(order).toEqual(['talk/tldr', 'mod/turn', 'talk/tldr'])
  isAway = false
  await $.turn.complete({ ...TURN, turnId: 't2', answer: long, reason: 'answer' })
  await w.clock.advance(10)
  expect(models.length).toBe(1)
  await $.turn.complete({ ...TURN, turnId: 't3', answer: 'short', reason: 'answer' })
  expect(w.filter(p => p.url.endsWith('/api/talk/tldr')).length).toBe(3)
})

test('catch me up: a digest message forks the session and posts the answer', async ($, on) => {
  const w = world(on, { inbox: [[{ seq: 70, kind: 'digest', id: 'd1', since: 1_791_000_000_000, after: 'tests pass' }]] })
  const prompts: string[] = []
  on('model.fork', ($, e) => {
    prompts.push(e.prompt)
    return { value: { isAnswered: true, text: ' Pushed the fix. Waiting on CI. ', usage: USAGE } }
  })
  await $.session.start(START)
  await w.clock.advance(60)
  await w.clock.advance(10)
  expect(w.find(p => p.url.endsWith('/api/mod/ack'))?.body.via).toBe('digest')
  expect(w.find(p => p.url.endsWith('/api/talk/digest'))?.body).toEqual({ id: 'd1', text: 'Pushed the fix. Waiting on CI.', session: 'sess-123', pane: 'w6:p1' })
  expect(prompts[0]).toContain('tests pass')
  expect(prompts[0]).toContain('2026-')
})

test('/phone is registered as an immediate command and relays to the server', async ($, on) => {
  const w = world(on, { posts: { '/api/talk/cmd': b => ({ ok: true, text: `did ${String(b.op)}` }) } })
  const regs: unknown[] = []
  on('command.register', ($, e) => {
    regs.push([e.name, e.immediate])
    return { value: { command: e.name } }
  })
  await $.session.start(START)
  expect(regs).toEqual([['phone', true]])
  const out = await $.command.run({ command: 'phone', args: ' send  hello there ' } as never)
  expect(out.text).toBe('did send')
  expect(w.find(p => p.url.endsWith('/api/talk/cmd'))?.body).toEqual({ op: 'send', text: 'hello there', session: 'sess-123', pane: 'w6:p1' })
})

function classicBottom(on: Parameters<typeof world>[0]): void {
  on('classic.PermissionRequest', () => ({}))
  on('classic.PermissionDenied', () => ({}))
}

const SUGGEST = [{ type: 'addRules', rules: [{ toolName: 'Bash', ruleContent: 'make deploy' }], behavior: 'allow', destination: 'session' }]

test('a permission dialog while away is answered on the phone (allow always = its suggestions)', async ($, on) => {
  let waits = 0
  const w = world(on, {
    posts: { '/api/talk/perm': () => ({ ok: true, id: 'p1' }) },
    gets: { '/api/talk/perm/p1/wait': () => (++waits < 2 ? { state: 'pending' } : { state: 'decided', decision: 'always' }) },
  })
  classicBottom(on)
  await $.session.start(START)
  const r = await $.classic.PermissionRequest({ tool_name: 'Bash', tool_input: { command: 'make deploy' }, permission_suggestions: SUGGEST } as never)
  expect(r.decision).toEqual({ behavior: 'allow', updatedPermissions: SUGGEST })
  expect(w.find(p => p.url.endsWith('/api/talk/perm'))?.body).toEqual({
    kind: 'ask', tool: 'Bash', input: { command: 'make deploy' }, always: true, session: 'sess-123', pane: 'w6:p1',
  })
})

test('a permission dialog at the desk (or with the server down) shows in the terminal', async ($, on) => {
  world(on, { posts: { '/api/talk/perm': () => ({ ok: true, desk: true }) } })
  classicBottom(on)
  await $.session.start(START)
  const r = await $.classic.PermissionRequest({ tool_name: 'Bash', tool_input: { command: 'make deploy' } } as never)
  expect(r.decision).toBeUndefined()
})

test('a phone deny is the dialog\'s deny; the user sitting down hands it back to the terminal', async ($, on) => {
  let next = 'deny'
  const w = world(on, {
    posts: { '/api/talk/perm': () => ({ ok: true, id: 'p2' }) },
    gets: { '/api/talk/perm/p2/wait': () => (next === 'deny' ? { state: 'decided', decision: 'deny' } : { state: 'desk' }) },
  })
  classicBottom(on)
  await $.session.start(START)
  const r = await $.classic.PermissionRequest({ tool_name: 'Write', tool_input: { file_path: '/etc/x' } } as never)
  expect(r.decision).toEqual({ behavior: 'deny', message: 'The user denied this from their phone.' })
  next = 'desk'
  const r2 = await $.classic.PermissionRequest({ tool_name: 'Write', tool_input: { file_path: '/etc/x' } } as never)
  expect(r2.decision).toBeUndefined()
  expect(w.some(p => p.url.endsWith('/api/talk/perm/p2/cancel'))).toBe(false)
})

test('an auto-mode denial goes to the phone; "allow once" tells the agent to retry and lets that call through once', async ($, on) => {
  const w = world(on, { inbox: [[{ seq: 80, kind: 'allow_once', tool: 'Bash', input: { command: 'rm -rf build' }, summary: 'rm -rf build' }]] })
  const submitted = capture(on)
  on('tool.check', () => ({ decision: 'ask' }))
  classicBottom(on)
  await $.session.start(START)
  await $.classic.PermissionDenied({ tool_name: 'Bash', tool_input: { command: 'rm -rf build' }, tool_use_id: 'tu1', reason: 'destructive' } as never)
  expect(w.find(p => p.url.endsWith('/api/talk/perm'))?.body).toEqual({
    kind: 'denied', tool: 'Bash', input: { command: 'rm -rf build' }, reason: 'destructive', session: 'sess-123', pane: 'w6:p1',
  })
  await w.clock.advance(60)
  expect(submitted.length).toBe(1)
  expect(submitted[0]).toContain('rm -rf build')
  // a query (no tool_use_id) never spends the allowance
  expect((await $.tool.check({ tool: 'Bash', input: { command: 'rm -rf build' } })).decision).toBe('ask')
})

test('phone messages stay in arrival order: held behind held, side-loop turns ignored, appended in order', async ($, on) => {
  const w = world(on, {
    inbox: [
      [{ seq: 1, kind: 'steer', text: 'order 1', working: true }],
      [{ seq: 2, kind: 'steer', text: 'order 2', working: true }],
      [{ seq: 3, kind: 'steer', text: 'order 3', working: true }, { seq: 4, kind: 'steer', text: 'order 4', working: true }],
      [{ seq: 5, kind: 'steer', text: 'order 5', working: true }],
    ],
  })
  const got: string[] = []
  on('prompt.submit', ($, e) => {
    got.push('submit:' + e.text)
    return { text: e.text }
  })
  on('turn.start', ($, e) => ({ turnId: e.turnId }))
  on('turn.complete', ($, e) => ({ text: e.answer }))
  on('tool.call', { tool: 'Read' }, () => ({ result: 'file' }))
  await $.session.start(START)
  await $.turn.start({ text: 'sleep 25', turnId: 'main' } as never)
  await w.clock.advance(60)
  // a side loop (auto-mode classifier, a fork) starts and ends inside the main turn
  await $.turn.start({ text: '', turnId: 'side' } as never)
  await $.turn.complete({ ...TURN, turnId: 'side', answer: '', reason: 'answer' })
  await w.clock.advance(60)
  await w.clock.advance(60)
  const vias = w.filter(p => p.url.endsWith('/api/mod/ack')).map(p => p.body.via)
  expect(vias).toEqual(['held', 'held', 'held', 'held'])
  expect(got).toEqual([])
  await $.tool.call({ tool: 'Read', file_path: '/a' })
  await w.clock.advance(10)
  // the four rows are dispatched in order as the tool returns; the kit refuses plugin appends, so each
  // falls back to a prompt, still in order (the append path itself is checked live on a throwaway agent)
  expect(w.filter(p => p.url.endsWith('/api/talk/held')).at(-1)?.body.held).toEqual([])
  expect(got).toEqual(['submit:order 1', 'submit:order 2', 'submit:order 3', 'submit:order 4'])
  await w.clock.advance(50)
  expect(w.filter(p => p.url.endsWith('/api/mod/ack')).map(p => p.body.via)).toEqual(['held', 'held', 'held', 'held', 'held'])
  await $.turn.complete({ ...TURN, turnId: 'main', answer: 'ok', reason: 'answer' })
  expect(got).toEqual(['submit:order 1', 'submit:order 2', 'submit:order 3', 'submit:order 4', 'submit:order 5'])
  // the side loop's end posted no turn record
  expect(w.filter(p => p.url.endsWith('/api/mod/turn')).map(p => p.body.turnId)).toEqual(['main'])
})

test('a message behind a pending submit is submitted too, never appended ahead of it', async ($, on) => {
  const w = world(on, { inbox: [[{ seq: 1, kind: 'steer', text: 'a', working: false }, { seq: 2, kind: 'steer', text: 'b', working: true }]] })
  const got: string[] = []
  on('prompt.submit', ($, e) => {
    got.push('submit:' + e.text)
    return new Promise(() => {}) as never // its turn has not started yet
  })
  on('session.append', ($, e) => {
    got.push('append')
    return { message: e.message, uuid: 'u' }
  })
  await $.session.start(START)
  await w.clock.advance(60)
  expect(got).toEqual(['submit:a', 'submit:b'])
})

test('no ask_phone tool: questions go through AskUserQuestion, which the guide points at', async ($, on) => {
  world(on)
  on('prompt.compose', () => ({ sections: [] }))
  const base = { model: 'm', promptModel: 'm', surfaces: [], outputStyle: null, traits: [] }
  const composed = await $.prompt.compose({ ...base, tools: ['mcp__sheprd__surface'] })
  expect(composed.sections.map(s => s.id)).toEqual(['sheprd:phone'])
  expect(composed.sections[0]?.text).toContain('AskUserQuestion')
})

test('the AskUserQuestion / plan dialogs are not turned into permission cards', async ($, on) => {
  const w = world(on, { posts: { '/api/talk/perm': () => ({ ok: true, id: 'p9' }) } })
  classicBottom(on)
  await $.session.start(START)
  const r = await $.classic.PermissionRequest({ tool_name: 'AskUserQuestion', tool_input: { questions: [] } } as never)
  expect(r.decision).toBeUndefined()
  expect(w.some(p => p.url.endsWith('/api/talk/perm'))).toBe(false)
})
