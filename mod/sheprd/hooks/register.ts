import type { EngineInterface, Register } from 'claude-code'

// sheprd: lets Claude Code put things on the user's phone.
//
// - registers the tool `surface` (the model calls it as mcp__sheprd__surface)
// - adds a short system-prompt section saying when to use it
// - forwards notes the built-in "You should know" mod shows above the prompt
// - forwards the built-in SendUserFile / PushNotification tools when a build offers them
// - auto-surfaces a screenshot a Bash command just wrote or a *screenshot* MCP tool returned
// - long-polls the server's inbox (GET /api/mod/inbox) for commands from the phone:
//   `steer` (appended into the running turn) and `compact`
// - reports each main-loop turn (POST /api/mod/turn) and the usage gauges (POST /api/mod/measure)
// - questions go through Claude's own AskUserQuestion, which the phone answers in full (talk.rs drives the dialog)
// - talking to agents (server/src/talk.rs): phone steers are held until the turn's next step so the
//   phone can take them back (`unsend`); Claude's own next-prompt guess → a chip; a TL;DR of long
//   answers when nobody is watching; "Catch me up" digests ($.model.fork); `/phone …` commands;
//   permission dialogs and auto-mode denials answered from the phone
//
// Everything fails soft: if the server is down the session never notices.

type Level = 'info' | 'attention' | 'done'
type Link = { url: string; label?: string }
type Surface = {
  session?: string
  pane?: string
  title: string
  body?: string
  level: Level
  images?: string[]
  links?: Link[]
  replies?: string[]
  command?: string
  // true only for the surface tool's own call: the server then waits for its transcript row before pushing;
  // anything else the mod surfaces is sent as false and pushes at once
  tool?: boolean
}

const PLUGIN = 'sheprd'
const TOOL = 'surface'
const TOOL_FULL = 'mcp__sheprd__surface'
const YSK = 'cc-plugin-you-should-know'
const AUTO_GAP_MS = 60_000
const POST_TIMEOUT_MS = 4_000
const HINT_TIMEOUT_MS = 1_500
const IMAGE_RE = /\.(png|jpe?g|webp|gif)$/i
const DEV_URL_RE = /\bhttps?:\/\/(localhost|127\.0\.0\.1|0\.0\.0\.0|\[::1?\]):(\d{2,5})(?![\d])/gi
const HTTP_CLIENT_RE = /(^|[\s;&|(])(curl|wget|xh|http|httpie|websocat)\s/
const DEV_SCAN_CHARS = 40_000
const DEV_MAX_PER_SESSION = 12

const DESCRIPTION = [
  "Put something on the user's phone. The user often supervises this session from the sheprd app on their phone",
  'instead of watching the terminal, so use this when something must be SEEN or needs them:',
  '- a screenshot or other visual result you produced (pass absolute image paths in `images`);',
  '- a dev server / preview URL you started or a page worth opening (`links`);',
  '- a finished deliverable at the end of a task (level `done`, with a one-line result in `title`);',
  '- a decision or blocker that needs the human before you can continue (level `attention`).',
  'Hand-offs: when the human must do something (run a sudo command, plug something in, log in), use level `attention`',
  'with the exact `command` to run and `replies: ["done"]`, then end your turn; their tap arrives as your next prompt.',
  'Give `replies` (one-tap answers) for any short question you ask through it.',
  'Keep `title` short (under ~60 chars); `body` is optional markdown, a few lines at most.',
  'Do not use it for routine progress or for things already obvious from your reply. At most a few calls per turn.',
].join('\n')

const INPUT_SCHEMA = {
  type: 'object',
  properties: {
    title: { type: 'string', description: 'Short headline shown in the notification / card.' },
    body: { type: 'string', description: 'Optional markdown details (keep it short).' },
    level: {
      type: 'string',
      enum: ['info', 'attention', 'done'],
      description: 'info: worth a look (no push). attention: needs the human. done: a finished result.',
    },
    images: {
      type: 'array',
      items: { type: 'string' },
      description: 'Absolute paths of local image files to show (screenshots, charts, renders).',
    },
    links: {
      type: 'array',
      items: {
        type: 'object',
        properties: { url: { type: 'string' }, label: { type: 'string' } },
        required: ['url'],
      },
      description: 'URLs to open, e.g. a dev server http://localhost:5173 (the phone reaches it over Tailscale).',
    },
    replies: {
      type: 'array',
      items: { type: 'string', maxLength: 40 },
      maxItems: 4,
      description: 'Up to 4 one-tap answers (each under 40 chars), e.g. ["done"]; the tapped one is sent to you as a prompt.',
    },
    command: {
      type: 'string',
      maxLength: 500,
      description: 'A command for the human to run themselves (shown with a Copy button), e.g. a sudo command.',
    },
  },
  required: ['title', 'level'],
}

// Decide (server/src/decide.rs, API.md "Decisions"): a choice for the user's Decide deck on the phone. Non-blocking:
// the pick arrives later as a message "[Decision: <title>] The user picked: <label> [<id>]" (rank: "The user ranked: 1. …").
const DECIDE_TOOL = 'decide'
const DECIDE_TOOL_FULL = 'mcp__sheprd__decide'
const DECIDE_DESCRIPTION = [
  "Ask the user to choose, on their phone's Decide deck (one question at a time, big tap targets, pictures side by side).",
  'Use it instead of a plain question whenever the answer is a choice between options, especially design options with',
  'screenshots: put each option\'s images on that option (absolute paths) so they are labelled. kinds: pick (one),',
  'rank (order some or all), multi (any number), yesno. The user may add a note or pick none with a note.',
  'It does not block: carry on with other work or end your turn; the answer arrives as your next prompt,',
  '"[Decision: <title>] The user picked: <label> [<id>]". Re-asking the same thing? Pass the same `key`: the old card is replaced.',
].join('\n')
const DECIDE_SCHEMA = {
  type: 'object',
  properties: {
    title: { type: 'string', description: 'The question, short (under ~70 chars), e.g. "Home layout: which direction?"' },
    body: { type: 'string', description: 'Optional context in a sentence or two (markdown).' },
    kind: { type: 'string', enum: ['pick', 'rank', 'multi', 'yesno'], description: 'Default pick.' },
    options: {
      type: 'array',
      maxItems: 8,
      items: {
        type: 'object',
        properties: {
          id: { type: 'string', description: 'Short id returned in the answer (default a, b, c…).' },
          label: { type: 'string', description: 'What the user sees, under 60 chars, e.g. "A · Briefing".' },
          sub: { type: 'string', description: 'Optional one-line detail.' },
          images: { type: 'array', items: { type: 'string' }, description: 'Absolute image paths for this option.' },
        },
        required: ['label'],
      },
      description: 'The options (yesno may omit them).',
    },
    key: { type: 'string', description: 'Stable id for this question: a newer card with the same key replaces the old one.' },
    deadlineMin: { type: 'number', description: 'Minutes until the question goes stale (optional).' },
    push: { type: 'boolean', description: 'false: no phone notification, it just waits in the deck (default true).' },
  },
  required: ['title'],
}

// Vault request (server/src/vault.rs, API.md "Vault requests"): a card with a paste field per secret in this agent's
// chat and on Home. The user pastes on the phone, the value goes straight to ~/.config/sheprd/vault/<name> (0600) and
// this session gets "[Vault] The user saved <name>" (names only). Non-blocking.
const VAULT_TOOL = 'vault_request'
const VAULT_TOOL_FULL = 'mcp__sheprd__vault_request'
const VAULT_DESCRIPTION = [
  'Ask the user for a secret (API key, token, client ID/secret, password) through their phone instead of chat.',
  'Their phone shows a card with one paste field per slot ("Save to vault", plus your `link`, e.g. the developer portal',
  'page where they get it). The value is written to ~/.config/sheprd/vault/<name> (owner-only file) and never passes',
  'through chat or this transcript. Never ask for secrets in plain chat and never echo them.',
  'It does not block: end your turn (or do other work). When saved you get a message "[Vault] The user saved <name>[, <name>]";',
  'then read the value yourself from ~/.config/sheprd/vault/<name> (e.g. `$(cat ~/.config/sheprd/vault/<name>)`',
  'inside the command that needs it; never print it). Ask for related values together (client ID + secret) via `slots`.',
].join('\n')
const VAULT_SCHEMA = {
  type: 'object',
  properties: {
    name: { type: 'string', description: 'Slot name, a-z 0-9 _ (1-64), e.g. "openai_api_key". Or use `slots` for several.' },
    label: { type: 'string', description: 'What the user sees for that slot, e.g. "OpenAI API key".' },
    slots: {
      type: 'array',
      maxItems: 6,
      items: {
        type: 'object',
        properties: {
          name: { type: 'string', description: 'Slot name, a-z 0-9 _ (1-64).' },
          label: { type: 'string', description: 'What the user sees, e.g. "X client ID".' },
          hint: { type: 'string', description: 'Optional placeholder, e.g. "starts with sk-".' },
        },
        required: ['name'],
      },
      description: 'Several secrets in one card, e.g. [{name:"x_client_id",label:"Client ID"},{name:"x_client_secret",label:"Client secret"}].',
    },
    why: { type: 'string', description: 'One or two sentences: what you need it for.' },
    hint: { type: 'string', description: 'Optional: where to find it, e.g. "Keys and tokens tab → OAuth 2.0 Client ID and Secret".' },
    link: { type: 'string', description: 'Optional https URL where the user gets it (shown as an Open link button).' },
    link_label: { type: 'string', description: 'Optional button text for `link`, e.g. "X developer portal".' },
    title: { type: 'string', description: 'Optional card headline (default "Paste <labels>").' },
  },
  required: ['why'],
}

const GUIDE =
  'The user often supervises this session from their phone (the sheprd app) rather than the terminal. ' +
  `When something must be seen rather than read, call ${TOOL_FULL}: screenshots or visual results you produced ` +
  '(absolute image paths), a dev server or preview URL you started (as a link), a finished deliverable (level done), ' +
  'or a decision/blocker that needs the human (level attention). ' +
  'If the human must act (sudo command, plug something in, log in): level attention, `command`, `replies: ["done"]`, then end your turn. ' +
  'Prefer one surfaced item over long prose describing it. ' +
  'When you need a decision from the user, ask with AskUserQuestion: they can answer it from the phone too. ' +
  "Don't spam: at most a few per turn, only things the user would want to open on their phone, never routine progress. " +
  `When you need a secret, token or key from the user, call ${VAULT_TOOL_FULL} instead of asking them to paste it in chat; never ask for secrets in plain chat.`

const HOME_BRIEF = [
  "You are the user's HOME agent: their general-purpose agent, empowered to do anything on their behalf. You are the",
  'one long-running session on the home screen of their sheprd phone app; take ownership of whatever they ask, end to end.',
  'Other Claude sessions on this machine are workers you can start, check on and direct.',
  '- Delegate freely: subagents for research and implementation, independent ones in parallel in the background.',
  '  Keep your own context lean (you are auto-compacted); keep durable facts in memory, not just in context.',
  '- Big, long-running or repo-specific work gets its own herdr tab with its own Claude so the user can watch and prompt it:',
  '  `herdr workspace list` to pick the workspace for that repo, `herdr tab create --workspace <ws> --cwd <dir> --label <name>`,',
  '  `herdr agent start <name> --kind claude --pane <pane> -- --permission-mode auto`, then `herdr agent prompt <pane> "<task>"`.',
  '  Always give the user the pane id (like w6:p3); the app turns it into a link. `herdr agent list` shows every agent.',
  '  Never send blind keystrokes (e.g. `send-keys <pane> 0` to dismiss a survey) before `agent prompt`: they get typed',
  '  into the prompt when nothing is there to dismiss. `herdr pane read <pane>` first and only send keys it actually needs.',
  '- Keep your auto-memory about the user current (preferences, projects, people, recurring tasks) and consult it.',
  '- Attached files arrive as paths like [attached: /path/file]; read them.',
  '- They read on a phone: lead with the answer, short paragraphs, no wide tables; surface screenshots, links and',
  `  finished results with ${TOOL_FULL}.`,
  '- When asked for status, check your subagents and the herdr agents and summarize briefly.',
  '- Questions about GPU/CPU/RAM/disk on this machine: run `curl -s 127.0.0.1:7878/api/res/brief` (`?disk=1` adds disk).',
  "  Answer from it in a few lines and keep its `#/r…` links and pane ids — they open the phone's resource view.",
].join('\n')

const HOME_TTL_MS = 60_000
const ANSWER_MAX_CHARS = 20_000

const INBOX_RETRY_MIN_MS = 2_000
const INBOX_RETRY_MAX_MS = 60_000
// Marks a phone steer appended into a running turn; the server shows such rows as the user's messages.
export const PHONE_STEER = '[from phone] '

type InboxMsg = {
  seq: number
  kind: string
  text?: unknown
  working?: unknown
  instructions?: unknown
  ref?: unknown
  id?: unknown
  since?: unknown
  after?: unknown
  tool?: unknown
  input?: unknown
  summary?: unknown
}
type Ack = { ok: boolean; via?: string; error?: string }
type Gauges = {
  context: { percent?: number; tokens?: number; window: number }
  rateLimits: readonly { kind: string; percentUsed: number; resetsAt?: string }[]
  cost?: { usd: number }
}

// Per load of the module (register runs again on a hot reload, which also drops the timers;
// the next hook then starts a fresh loop).
type Runtime = {
  baseUrl: string
  inbox?: { since: number; failures: number }
  isTurnRunning: boolean
  // a compaction the phone asked for while a turn ran (run once it ends)
  pendingCompact?: { instructions?: string }
  // phone steers held until the turn's next step (the phone can still take them back); strictly FIFO
  held: Held[]
  // prompt.submit calls not yet started as a turn (later phone messages queue behind them)
  submitting: number
  // the main loop's turn: the first turn started while none was open (side loops start inside it)
  mainTurn?: string
  // a release of held steers is scheduled (they were held while no turn was known to run)
  isReleaseScheduled: boolean
  // calls auto mode denied that the phone allowed once
  allowances: { tool: string; key: string; until: number }[]
}
type Held = { seq: number; text: string; ts: number }

// Talking to agents (server/src/talk.rs)
const TLDR_MIN_CHARS = 1500
const TLDR_MODEL = 'haiku'
const PERM_WAIT_MS = 15 * 60_000
const PERM_POLL_MS = 10_000
const ALLOW_ONCE_TTL_MS = 15 * 60_000
const PHONE_CMD = 'phone'
// tools whose permission request is their own dialog (answered on the phone through its own card)
const DIALOG_TOOLS = new Set(['AskUserQuestion', 'ExitPlanMode', 'EnterPlanMode'])

export const register: Register = (on, options) => {
  const baseUrl = String(options.baseUrl ?? 'http://127.0.0.1:7878').replace(/\/+$/, '')
  const isAuto = options.autoSurface !== false
  const isForwardingYsk = options.forwardYouShouldKnow !== false
  const forwarded = new Set<string>()
  const devPorts = new Set<string>()
  const ownPort = portOf(baseUrl)
  const rt: Runtime = { baseUrl, isTurnRunning: false, held: [], allowances: [], submitting: 0, isReleaseScheduled: false }
  let lastAutoAt = 0

  function takeAutoSlot(): boolean {
    const now = Date.now()
    if (now - lastAutoAt < AUTO_GAP_MS) return false
    lastAutoAt = now
    return true
  }

  // (a) the tool the model calls
  on('session.start', async ($, e, next) => {
    await $.tool.register({ name: TOOL, description: DESCRIPTION, inputSchema: INPUT_SCHEMA })
    await $.tool.register({ name: DECIDE_TOOL, description: DECIDE_DESCRIPTION, inputSchema: DECIDE_SCHEMA })
    await $.tool.register({ name: VAULT_TOOL, description: VAULT_DESCRIPTION, inputSchema: VAULT_SCHEMA })
    await $.command
      .register({
        name: PHONE_CMD,
        description: 'sheprd phone: send <text> · mute · unmute · pin · unpin · away · back',
        argumentHint: 'send <text> | mute | unmute | pin | unpin | away | back',
        immediate: true,
      })
      .catch(() => undefined)
    await startInbox($, rt)
    return next(e)
  })

  // /phone …: works mid-turn (immediate)
  on('command.run', { command: 'phone' }, async ($, e) => ({ text: await phoneCommand($, rt, e.args) }))

  // Claude's own guess at the next prompt → the phone's first chip
  on('prompt.suggest', async ($, e, next) => {
    const shown = await next(e)
    if (e.origin.kind === 'suggestion' && rt.inbox && e.text.trim()) {
      await postJson($, `${rt.baseUrl}/api/talk/suggest`, { text: e.text.trim().slice(0, 300) }, HINT_TIMEOUT_MS)
    }
    return shown
  })

  // A call auto mode denied and the phone allowed once runs this time without asking.
  on('tool.check', async ($, e, next) => {
    const verdict = await next(e)
    if (verdict.decision !== 'ask' || e.tool_use_id === undefined || rt.allowances.length === 0) return verdict
    const now = Date.now()
    rt.allowances = rt.allowances.filter(a => a.until > now)
    const key = permKey(e.tool, e.input)
    const k = rt.allowances.findIndex(a => a.tool === e.tool && a.key === key)
    if (k < 0) return verdict
    rt.allowances.splice(k, 1)
    return { decision: 'allow', reason: 'Allowed once by the user from the sheprd phone app.' }
  })

  // A permission dialog: the terminal shows it while this hook runs, and the phone offers Allow / Deny
  // (/ Allow always) at the same time; whichever answers first decides.
  on('classic.PermissionRequest', async ($, e, next) => {
    const r = await next(e)
    // AskUserQuestion / plan approval dialogs are permission requests too, but their dialog IS the answer
    // (the phone answers those through their own cards)
    if (r.decision || r.block !== undefined || !rt.inbox || DIALOG_TOOLS.has(e.tool_name)) return r
    const always = Array.isArray(e.permission_suggestions) && e.permission_suggestions.length > 0
    const d = await permFromPhone($, rt, e.tool_name, e.tool_input, always, next.signal)
    if (d === 'allow') return { ...r, decision: { behavior: 'allow' } }
    if (d === 'always' && e.permission_suggestions) {
      return { ...r, decision: { behavior: 'allow', updatedPermissions: e.permission_suggestions } }
    }
    if (d === 'deny') return { ...r, decision: { behavior: 'deny', message: 'The user denied this from their phone.' } }
    return r
  })

  // Auto mode refused a call: the phone offers "Allow once".
  on('classic.PermissionDenied', async ($, e, next) => {
    const r = await next(e)
    if (rt.inbox) {
      await postJson(
        $,
        `${rt.baseUrl}/api/talk/perm`,
        { kind: 'denied', tool: e.tool_name, input: e.tool_input, reason: e.reason },
        HINT_TIMEOUT_MS,
      )
    }
    return r
  })

  on('session.measure', async ($, e, next) => {
    const measured = await next(e)
    await postGauges($, rt, e)
    return measured
  })

  // The phone's inbox: started at session.start, restarted by the next turn after a hot reload.
  on('turn.start', async ($, e, next) => {
    if (!rt.isTurnRunning) rt.mainTurn = e.turnId
    rt.isTurnRunning = true
    await startInbox($, rt)
    return next(e)
  })

  on('turn.complete', async ($, e, next) => {
    const done = await next(e)
    if (e.agentId !== undefined) return done
    // a side loop (a fork) that ran inside the main turn: not the main loop's end
    if (rt.isTurnRunning && rt.mainTurn !== undefined && e.turnId !== rt.mainTurn) return done
    rt.isTurnRunning = false
    rt.mainTurn = undefined
    // steers still held when the turn ends: their own turn (or, after an interrupt, back into the box like Claude's queue)
    if (rt.held.length > 0) await releaseHeld($, rt, e.reason === 'aborted' ? 'fill' : 'submit')
    let isTldrWanted = false
    if (rt.inbox && e.reason === 'answer' && e.answer.length > TLDR_MIN_CHARS) {
      const r = await postTalk($, rt, '/api/talk/tldr', { turnId: e.turnId, pending: true })
      isTldrWanted = r?.away === true
    }
    if (rt.inbox) {
      const record: Record<string, unknown> = {
        turnId: e.turnId,
        answer: e.answer.slice(-ANSWER_MAX_CHARS),
        durationMs: e.durationMs,
        reason: e.reason,
      }
      if (e.reason === 'refusal') {
        const why = [e.refusal.category, e.refusal.explanation].filter(Boolean).join(': ')
        if (why) record.error = why
      }
      if (e.usage) record.usage = e.usage
      await postJson($, `${rt.baseUrl}/api/mod/turn`, record, HINT_TIMEOUT_MS)
    }
    if (isTldrWanted) {
      const { turnId, answer } = e
      $.clock.after(0, () => void writeTldr($, rt, turnId, answer))
    }
    const pending = rt.pendingCompact
    if (pending) {
      rt.pendingCompact = undefined
      $.clock.after(1_000, () => void runCompact($, rt, pending.instructions))
    }
    return done
  })

  on('tool.call', { tool: TOOL_FULL }, async ($, e) => {
    const title = typeof e.title === 'string' && e.title.trim() ? e.title.trim() : 'Claude wants you to see this'
    const level: Level = e.level === 'attention' || e.level === 'done' ? e.level : 'info'
    const cwd = await $.session.cwd().catch(() => undefined)
    const images: string[] = []
    for (const p of Array.isArray(e.images) ? e.images : []) {
      if (typeof p === 'string' && p.trim()) images.push(await absolute($, p.trim(), cwd))
    }
    const links: Link[] = []
    for (const l of Array.isArray(e.links) ? e.links : []) {
      if (typeof l === 'string') links.push({ url: l })
      else if (l && typeof l === 'object' && typeof (l as Link).url === 'string') {
        const { url, label } = l as Link
        links.push(typeof label === 'string' ? { url, label } : { url })
      }
    }
    const item: Surface = { title, level, tool: true }
    if (typeof e.body === 'string' && e.body.trim()) item.body = e.body
    if (images.length > 0) item.images = images
    if (links.length > 0) item.links = links
    const replies = (Array.isArray(e.replies) ? e.replies : [])
      .filter((r): r is string => typeof r === 'string' && r.trim() !== '')
      .map(r => r.trim().slice(0, 40))
      .slice(0, 4)
    if (replies.length > 0) item.replies = replies
    if (typeof e.command === 'string' && e.command.trim()) item.command = e.command.trim().slice(0, 500)
    const sent = await post($, baseUrl, item)
    return {
      result: sent.ok
        ? `Surfaced to the user's phone${sent.id ? ` (id ${sent.id})` : ''}.` +
          (item.replies ? ' If you are waiting on them, end your turn now; their tap arrives as your next prompt.' : '')
        : `Noted. (The phone app is not reachable right now: ${sent.why}; nothing else to do, carry on.)`,
    }
  })

  on('tool.call', { tool: DECIDE_TOOL_FULL }, async ($, e) => {
    const title = typeof e.title === 'string' && e.title.trim() ? e.title.trim() : ''
    if (!title) return { result: 'decide needs a title (the question).', isError: true }
    const cwd = await $.session.cwd().catch(() => undefined)
    const options: Record<string, unknown>[] = []
    for (const o of Array.isArray(e.options) ? e.options : []) {
      if (typeof o === 'string') { options.push({ label: o }); continue }
      if (!o || typeof o !== 'object') continue
      const x = o as Record<string, unknown>
      const images: string[] = []
      for (const p of Array.isArray(x.images) ? x.images : []) {
        if (typeof p === 'string' && p.trim()) images.push(await absolute($, p.trim(), cwd))
      }
      options.push({ id: x.id, label: x.label, sub: x.sub, images })
    }
    const decision: Record<string, unknown> = { kind: typeof e.kind === 'string' ? e.kind : 'pick', options }
    if (typeof e.key === 'string' && e.key.trim()) decision.key = e.key.trim()
    if (typeof e.deadlineMin === 'number' && e.deadlineMin > 0) decision.deadlineMin = Math.round(e.deadlineMin)
    const body: Record<string, unknown> = { title, level: 'attention', tool: true, decision }
    if (typeof e.body === 'string' && e.body.trim()) body.body = e.body
    if (e.push === false) body.push = false
    const sent = await postFull($, `${baseUrl}/api/surface`, body, POST_TIMEOUT_MS)
    if (!sent.ok) {
      const err = typeof sent.json.error === 'string' ? sent.json.error : sent.why
      return { result: `Not posted: ${err}.`, isError: true }
    }
    const id = typeof sent.json.id === 'string' ? sent.json.id : ''
    return {
      result:
        `Posted to the user's Decide deck${id ? ` (id ${id})` : ''}. The answer arrives as a later prompt starting ` +
        '"[Decision: ". Continue other work or end your turn; do not ask the same question again in chat.' +
        (id ? ` To take it back: curl -s -X POST ${baseUrl}/api/decide/${id}/withdraw` : ''),
    }
  })

  on('tool.call', { tool: VAULT_TOOL_FULL }, async ($, e) => {
    const str = (v: unknown, n: number): string | undefined => (typeof v === 'string' && v.trim() ? v.trim().slice(0, n) : undefined)
    const body: Record<string, unknown> = { why: str(e.why, 400) ?? '' }
    if (Array.isArray(e.slots) && e.slots.length > 0) {
      body.slots = e.slots.map(x => {
        if (typeof x === 'string') return { name: x }
        const o = (x && typeof x === 'object' ? x : {}) as Record<string, unknown>
        return { name: o.name, label: str(o.label, 60), hint: str(o.hint, 140) }
      })
    } else if (typeof e.name === 'string') {
      body.name = e.name.trim()
      if (str(e.label, 60)) body.label = str(e.label, 60)
    } else {
      return { result: 'vault_request needs `name` (or `slots`): the vault slot(s) to fill, a-z 0-9 _.', isError: true }
    }
    for (const [k, to, n] of [['hint', 'hint', 200], ['link', 'link', 500], ['link_label', 'linkLabel', 30], ['title', 'title', 70]] as const) {
      const v = str(e[k], n)
      if (v) body[to] = v
    }
    const sent = await postFull($, `${baseUrl}/api/vault/request`, body, POST_TIMEOUT_MS)
    if (!sent.ok) {
      const err = typeof sent.json.error === 'string' ? sent.json.error : sent.why
      return { result: `Vault request not posted: ${err}.`, isError: true }
    }
    const id = typeof sent.json.id === 'string' ? sent.json.id : ''
    const names = Array.isArray(body.slots) ? (body.slots as { name?: unknown }[]).map(x => String(x.name)) : [String(body.name)]
    return {
      result:
        `Asked the user on their phone for ${names.join(', ')}${id ? ` (request ${id})` : ''}. End your turn now (or do other work); ` +
        'when they save it you get "[Vault] The user saved <name>". Then read it from ' +
        names.map(n => `~/.config/sheprd/vault/${n}`).join(', ') +
        ' inside the command that needs it, without printing it. Do not ask for it in chat.' +
        (id ? ` To take the request back: curl -s -X POST ${baseUrl}/api/vault/request/${id}/cancel` : ''),
    }
  })

  // (b) the system prompt section, only where the tool is offered
  on('prompt.compose', async ($, e, next) => {
    const composed = await next(e)
    if (!e.tools.includes(TOOL_FULL)) return composed
    const sections = [...composed.sections, { id: `${PLUGIN}:phone`, text: GUIDE, scope: 'session' as const }]
    if (await isHome($, baseUrl)) sections.push({ id: `${PLUGIN}:home`, text: HOME_BRIEF, scope: 'session' as const })
    return { sections }
  })

  // Bridge: the built-in "You should know" mod keeps the note it shows above the prompt
  // in its store under "sessions" as { id, view: { kind: 'offer', line, explanation, shownAt } }.
  // Its $.store.set passes through this (user-tier, outer) hook on its way down.
  on('store.set', { key: 'sessions' }, async ($, e, next) => {
    const written = await next(e)
    if (!isForwardingYsk || next.origin.plugin !== YSK || !Array.isArray(e.value)) return written
    for (const entry of e.value as unknown[]) {
      if (!entry || typeof entry !== 'object') continue
      const { id, view } = entry as { id?: unknown; view?: unknown }
      if (!view || typeof view !== 'object') continue
      const v = view as { kind?: unknown; line?: unknown; explanation?: unknown; shownAt?: unknown }
      if (v.kind !== 'offer' || typeof v.line !== 'string' || typeof v.shownAt !== 'number') continue
      if (Date.now() - v.shownAt > 60_000) continue
      const key = `${String(id)}:${v.shownAt}:${v.line}`
      if (forwarded.has(key)) continue
      forwarded.add(key)
      const item: Surface = { title: v.line, level: 'attention' }
      if (typeof id === 'string') item.session = id
      if (typeof v.explanation === 'string' && v.explanation.trim()) item.body = `**You should know** - ${v.explanation}`
      else item.body = '**You should know**'
      await post($, baseUrl, item)
    }
    return written
  })

  // Bridge: built-in tools some builds offer (Remote Control / brief mode)
  on('tool.call', { tool: 'SendUserFile' }, async ($, e, next) => {
    const ran = await next(e)
    if (ran.deny !== undefined || ran.isError === true) return ran
    const cwd = await $.session.cwd().catch(() => undefined)
    const files = await Promise.all(e.files.map(f => absolute($, f, cwd)))
    const images = files.filter(f => IMAGE_RE.test(f))
    const others = files.filter(f => !IMAGE_RE.test(f))
    const item: Surface = {
      title: e.caption?.trim() || (files.length === 1 ? `File: ${files[0]?.split('/').pop()}` : `${files.length} files`),
      level: e.status === 'proactive' ? 'done' : 'info',
    }
    if (images.length > 0) item.images = images
    if (others.length > 0) item.body = others.map(f => `- \`${f}\``).join('\n')
    await post($, baseUrl, item)
    return ran
  })

  // (d) AskUserQuestion: tell the phone what is being asked while the dialog is open
  on('tool.call', { tool: 'AskUserQuestion' }, async ($, e, next) => {
    const questions = Array.isArray(e.questions) ? e.questions : []
    if (questions.length === 0) return next(e)
    await hint($, baseUrl, { kind: 'ask', questions })
    try {
      return await next(e)
    } finally {
      await hint($, baseUrl, { kind: 'ask_clear' })
    }
  })

  // (d) ExitPlanMode: send the plan to the phone's plan card while the approval menu is open
  on('tool.call', { tool: 'ExitPlanMode' }, async ($, e, next) => {
    const input = e as unknown as { plan?: unknown; planFilePath?: unknown }
    const path = typeof input.planFilePath === 'string' ? input.planFilePath : undefined
    let plan = typeof input.plan === 'string' ? input.plan : ''
    if (!plan.trim() && path) plan = await $.fs.read(path).then(t => (typeof t === 'string' ? t : ''), () => '')
    if (!plan.trim()) return next(e)
    await hint($, baseUrl, path ? { kind: 'plan', plan, path } : { kind: 'plan', plan })
    try {
      return await next(e)
    } finally {
      await hint($, baseUrl, { kind: 'plan_clear' })
    }
  })

  on('tool.call', { tool: 'PushNotification' }, async ($, e, next) => {
    const ran = await next(e)
    if (ran.deny !== undefined || ran.isError === true) return ran
    await post($, baseUrl, { title: e.message.slice(0, 200), level: 'attention' })
    return ran
  })

  // (c) auto-surface: an image file a Bash command just wrote
  on('tool.call', { tool: 'Bash' }, async ($, e, next) => {
    const startedAt = Date.now()
    const ran = await next(e)
    if (!isAuto || ran.deny !== undefined || ran.isError === true) return ran
    const candidates = [...e.command.matchAll(/(?:^|[\s'"=>])((?:~\/|\/|\.{1,2}\/)?[\w@%+.,/-]*\.(?:png|jpe?g|webp))(?=$|[\s'";&|)])/gi)]
      .map(m => m[1])
      .filter((p): p is string => typeof p === 'string' && p.length > 4)
    if (candidates.length === 0) return ran
    const cwd = await $.session.cwd().catch(() => undefined)
    for (const p of candidates.reverse()) {
      const abs = await absolute($, p, cwd)
      if (!(await isFreshFile($, abs, startedAt))) continue
      if (!takeAutoSlot()) break
      const shot: Surface = { title: `Screenshot: ${abs.split('/').pop()}`, level: 'info', images: [abs] }
      await post($, baseUrl, shot)
      break
    }
    return ran
  })

  // (c) auto-surface: a dev server URL a foreground Bash command printed (or started), once per port per session
  on('tool.call', { tool: 'Bash' }, async ($, e, next) => {
    const ran = await next(e)
    try {
      if (!isAuto || ran.deny !== undefined || ran.isError === true || e.run_in_background === true) return ran
      const out = ran.result as { stdout?: unknown; stderr?: unknown; backgroundTaskId?: unknown } | undefined
      if (out && typeof out === 'object' && typeof out.backgroundTaskId === 'string') return ran
      const texts = [
        typeof out?.stdout === 'string' ? out.stdout.slice(-DEV_SCAN_CHARS) : '',
        typeof out?.stderr === 'string' ? out.stderr.slice(-DEV_SCAN_CHARS) : '',
      ]
      // A URL in the command counts unless the command is just a client hitting it (curl, wget, ...).
      if (!HTTP_CLIENT_RE.test(e.command)) texts.unshift(e.command)
      const found = findDevUrls(texts.join('\n')).filter(d => d.port !== ownPort)
      if (found.length === 0) return ran
      const session = (await $.session.id().catch(() => undefined)) ?? ''
      const fresh = found.find(d => !devPorts.has(`${session}:${d.port}`))
      if (!fresh || devPorts.size >= DEV_MAX_PER_SESSION) return ran
      devPorts.add(`${session}:${fresh.port}`)
      await post($, baseUrl, {
        title: `Dev server :${fresh.port}`,
        level: 'info',
        links: [{ url: fresh.url, label: `:${fresh.port}` }],
      })
    } catch {}
    return ran
  })

  // Every tool call. (1) A held phone steer is appended once the tool the main loop is running
  // returns, right before the loop's next request reads it; until then the phone can take it back.
  // (2) auto-surface: an image a *screenshot* MCP tool returned (playwright, chrome-devtools, ...)
  on('tool.call', async ($, e, next) => {
    const ran = await next(e)
    // The rows are dispatched before this hook returns, so the request the loop builds from this result
    // reads them (dispatched later, they can miss it); not awaited: their store waits on this result's row.
    if (e.agentId === undefined && rt.held.length > 0) appendHeld($, rt)
    const tool = String(e.tool)
    if (!isAuto || !tool.startsWith('mcp__') || tool === TOOL_FULL || !/screenshot/i.test(tool)) return ran
    if (ran.deny !== undefined || ran.isError === true) return ran
    const image = findImage(ran.result)
    if (!image || !takeAutoSlot()) return ran
    try {
      const home = (await $.env.get('HOME')) ?? '/tmp'
      const dir = `${home}/.cache/sheprd/shots`
      const ext = image.mime.includes('jpeg') ? 'jpg' : image.mime.includes('webp') ? 'webp' : 'png'
      const path = `${dir}/${e.tool_use_id.replace(/[^\w-]/g, '')}.${ext}`
      const wrote = await $.process.run(['sh', '-c', 'mkdir -p "$1" && base64 -d > "$2"', 'sh', dir, path], {
        stdin: image.data,
        timeoutMs: 10_000,
      })
      if (wrote.exitCode === 0) {
        const short = tool.replace(/^mcp__/, '').replace(/__/g, ' ')
        await post($, baseUrl, { title: `Screenshot (${short})`, level: 'info', images: [path] })
      }
    } catch {}
    return ran
  })
}

// Starts the inbox long-poll once per module load, in herdr panes only (the phone acts on panes).
async function startInbox($: EngineInterface, rt: Runtime): Promise<void> {
  if (rt.inbox) return
  const pane = await $.env.get('HERDR_PANE_ID').catch(() => undefined)
  if (!pane || rt.inbox) return
  rt.inbox = { since: 0, failures: 0 }
  $.clock.after(50, () => void pollInbox($, rt))
}

// One long-poll (the server holds it up to 25 s), then the next; backs off while the server is down.
async function pollInbox($: EngineInterface, rt: Runtime): Promise<void> {
  const st = rt.inbox
  const baseUrl = rt.baseUrl
  if (!st) return
  let delay = 50
  try {
    const [session, pane] = await Promise.all([$.session.id(), $.env.get('HERDR_PANE_ID').catch(() => undefined)])
    const q = `session=${encodeURIComponent(session)}&since=${st.since}${pane ? `&pane=${encodeURIComponent(pane)}` : ''}`
    const res = await $.http.fetch(`${baseUrl}/api/mod/inbox?${q}`)
    if (!res.ok) throw new Error(`HTTP ${res.status}`)
    const parsed = JSON.parse(res.text) as { messages?: unknown }
    st.failures = 0
    for (const m of Array.isArray(parsed.messages) ? parsed.messages : []) {
      if (!m || typeof m !== 'object' || typeof (m as InboxMsg).seq !== 'number') continue
      const msg = m as InboxMsg
      if (msg.seq <= st.since) continue
      st.since = msg.seq
      const ack = await handleInbox($, rt, msg)
      await postJson($, `${baseUrl}/api/mod/ack`, { seq: msg.seq, ...ack }, HINT_TIMEOUT_MS)
    }
  } catch {
    st.failures += 1
    delay = Math.min(INBOX_RETRY_MAX_MS, INBOX_RETRY_MIN_MS * 2 ** Math.min(st.failures - 1, 6))
  }
  $.clock.after(delay, () => void pollInbox($, rt))
}

// Runs one command from the phone; never throws.
async function handleInbox($: EngineInterface, rt: Runtime, m: InboxMsg): Promise<Ack> {
  try {
    if (m.kind === 'steer') {
      const text = typeof m.text === 'string' ? m.text.trim() : ''
      if (!text) return { ok: false, error: 'empty' }
      return await deliver($, rt, m.seq, text, m.working === true)
    }
    if (m.kind === 'unsend') {
      const k = rt.held.findIndex(h => h.seq === m.ref)
      if (k < 0) return { ok: false, error: 'already_sent' }
      rt.held.splice(k, 1)
      await postHeld($, rt)
      return { ok: true, via: 'unsent' }
    }
    if (m.kind === 'digest') {
      const id = typeof m.id === 'string' ? m.id : ''
      if (!id) return { ok: false, error: 'no id' }
      const since = typeof m.since === 'number' ? m.since : undefined
      const after = typeof m.after === 'string' ? m.after : undefined
      $.clock.after(0, () => void writeDigest($, rt, id, since, after))
      return { ok: true, via: 'digest' }
    }
    if (m.kind === 'allow_once') {
      const tool = typeof m.tool === 'string' ? m.tool : ''
      if (!tool) return { ok: false, error: 'no tool' }
      rt.allowances.push({ tool, key: permKey(tool, m.input), until: Date.now() + ALLOW_ONCE_TTL_MS })
      const what = typeof m.summary === 'string' && m.summary ? m.summary : tool
      const text =
        `I allowed this from my phone: ${tool} \`${what.slice(0, 300)}\` (auto mode had blocked it). ` +
        'Retry that exact call now if you still need it.'
      return await deliver($, rt, m.seq, text, false)
    }
    if (m.kind === 'compact') {
      const instructions = typeof m.instructions === 'string' && m.instructions.trim() ? m.instructions.trim() : undefined
      if (rt.isTurnRunning) {
        rt.pendingCompact = { instructions }
        return { ok: true, via: 'deferred' }
      }
      void runCompact($, rt, instructions)
      return { ok: true, via: 'compact' }
    }
    return { ok: false, error: `unknown kind ${String(m.kind)}` }
  } catch (err) {
    return { ok: false, error: err instanceof Error ? err.message : String(err) }
  }
}

// One phone message, strictly in arrival order: behind a pending submit it is submitted too; while a
// turn runs (or anything is already held) it is held until the main loop's next step; else submitted.
async function deliver($: EngineInterface, rt: Runtime, seq: number, text: string, isWorking: boolean): Promise<Ack> {
  if (rt.submitting > 0 && rt.held.length === 0) {
    submitInOrder($, rt, text)
    return { ok: true, via: 'submit' }
  }
  if (rt.isTurnRunning || isWorking || rt.held.length > 0) {
    rt.held.push({ seq, text, ts: Date.now() })
    await postHeld($, rt)
    // held while no turn is known to run (the server saw one): released soon unless a turn claims it
    if (!rt.isTurnRunning && !rt.isReleaseScheduled) {
      rt.isReleaseScheduled = true
      $.clock.after(3_000, () => {
        rt.isReleaseScheduled = false
        if (!rt.isTurnRunning && rt.held.length > 0) void releaseHeld($, rt, 'submit')
      })
    }
    return { ok: true, via: 'held' }
  }
  submitInOrder($, rt, text)
  return { ok: true, via: 'submit' }
}

// A prompt of its own; counted until its turn starts so later messages queue behind it.
function submitInOrder($: EngineInterface, rt: Runtime, text: string): void {
  rt.submitting += 1
  $.prompt
    .submit({ text })
    .catch(() => undefined)
    .finally(() => {
      rt.submitting -= 1
    })
}

// Releases every held steer, oldest first: `append` (the main loop's tool just returned, so its next
// request reads them; behind a pending submit they are submitted instead), `submit` (one prompt) or
// `fill` (back into the prompt box, as Claude does with its queue after an interrupt).
async function releaseHeld($: EngineInterface, rt: Runtime, how: 'append' | 'submit' | 'fill'): Promise<void> {
  const held = rt.held.splice(0)
  if (held.length === 0) return
  await postHeld($, rt)
  const texts = held.map(h => h.text)
  if (how === 'fill') {
    await $.prompt.fill({ text: texts.join('\n'), mode: 'append' }).catch(() => undefined)
    return
  }
  if (how === 'append' && rt.submitting === 0) {
    for (let k = 0; k < texts.length; k++) {
      if (await appendRow($, texts[k] ?? '')) continue
      // could not append: the rest goes as one prompt, still in order
      submitInOrder($, rt, texts.slice(k).join('\n'))
      return
    }
    return
  }
  submitInOrder($, rt, texts.join('\n'))
}

// Dispatches every held steer as a user row, oldest first, without waiting for the stores; a row
// refused is submitted as a prompt of its own instead (behind any earlier submit).
function appendHeld($: EngineInterface, rt: Runtime): void {
  if (rt.submitting > 0) {
    void releaseHeld($, rt, 'submit')
    return
  }
  const held = rt.held.splice(0)
  const rows = held.map(h => appendRow($, h.text))
  void (async () => {
    await postHeld($, rt)
    for (let k = 0; k < rows.length; k++) if (!(await rows[k])) submitInOrder($, rt, held[k]?.text ?? '')
  })()
}

// A user row the running turn reads at its next step; false when refused.
async function appendRow($: EngineInterface, text: string): Promise<boolean> {
  try {
    const row = await $.session.append({ message: { type: 'user', content: [{ type: 'text', text: PHONE_STEER + text }] } })
    return !('deny' in row && row.deny)
  } catch {
    return false
  }
}

// What the phone shows as queued (with × to take it back).
async function postHeld($: EngineInterface, rt: Runtime): Promise<void> {
  await postJson($, `${rt.baseUrl}/api/talk/held`, { held: rt.held.map(h => ({ ...h })) }, HINT_TIMEOUT_MS)
}

// POSTs to the server and returns its JSON reply (undefined on any failure).
async function postTalk(
  $: EngineInterface,
  rt: Runtime,
  path: string,
  body: Record<string, unknown>,
): Promise<Record<string, unknown> | undefined> {
  try {
    const [session, pane] = await Promise.all([$.session.id().catch(() => undefined), $.env.get('HERDR_PANE_ID').catch(() => undefined)])
    const res = await Promise.race([
      $.http.fetch(`${rt.baseUrl}${path}`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ ...body, session, pane }),
      }),
      $.clock.sleep(POST_TIMEOUT_MS).then(() => null),
    ])
    if (!res || !res.ok) return undefined
    const v: unknown = JSON.parse(res.text)
    return v && typeof v === 'object' ? (v as Record<string, unknown>) : undefined
  } catch {
    return undefined
  }
}

// A short synopsis of a long answer (the user is away): the phone pins it above the answer and pushes it.
async function writeTldr($: EngineInterface, rt: Runtime, turnId: string, answer: string): Promise<void> {
  const prompt =
    'Write a TL;DR of the assistant answer below for someone reading it on a phone lock screen. ' +
    'At most 2 short sentences, under 220 characters, plain text (no markdown, no preamble). ' +
    'Lead with the outcome, or with the question the reader has to answer.\n\n<answer>\n' +
    answer.slice(0, 30_000) +
    '\n</answer>'
  const r = await $.model.complete({ model: TLDR_MODEL, prompt, maxTokens: 200, timeoutMs: 25_000 }).catch(() => undefined)
  const text = r && r.isAnswered ? r.text.trim().replace(/^TL;?DR:?\s*/i, '') : ''
  await postJson(
    $,
    `${rt.baseUrl}/api/talk/tldr`,
    text ? { turnId, text, answer: answer.slice(-ANSWER_MAX_CHARS) } : { turnId, error: r ? (r.isAnswered ? 'empty' : r.reason) : 'failed' },
    POST_TIMEOUT_MS,
  )
}

// "Catch me up": one tool-less question over the session's own (cached) transcript.
async function writeDigest($: EngineInterface, rt: Runtime, id: string, since?: number, after?: string): Promise<void> {
  const when = since ? new Date(since).toISOString() : undefined
  const prompt = [
    '(A check-in from the user\'s phone. Answer only this; do not continue the task or call tools.)',
    when
      ? `The user last looked at this conversation at ${when}${after ? ` (the last thing they saw: "${after}")` : ''}. It is now ${new Date().toISOString()}.`
      : `It is now ${new Date().toISOString()}.`,
    `In at most 5 short lines of plain text (no headings, no tables): what changed${when ? ' since then' : ' recently'},`,
    'where things stand (done / in progress / blocked), and what you need from them, if anything. Lead with the most important point.',
  ].join('\n')
  const r = await $.model.fork({ prompt }).catch(() => undefined)
  const body: Record<string, unknown> = { id }
  if (r && r.isAnswered && r.text.trim()) body.text = r.text.trim()
  else body.error = r && !r.isAnswered ? (r.reason === 'nothing-to-fork' ? 'Nothing to catch up on yet.' : `No reply (${r.reason}).`) : 'Failed.'
  await postJson($, `${rt.baseUrl}/api/talk/digest`, body, POST_TIMEOUT_MS)
}

// /phone send <text> | mute | unmute | pin | unpin | away | back
async function phoneCommand($: EngineInterface, rt: Runtime, args: string): Promise<string> {
  const trimmed = args.trim()
  const op = (trimmed.split(/\s+/)[0] ?? '').toLowerCase()
  const text = trimmed.slice(op.length).trim()
  const r = await postTalk($, rt, '/api/talk/cmd', { op, text })
  if (!r) return 'The sheprd server is not reachable.'
  return typeof r.text === 'string' ? r.text : 'Done.'
}

// Identity of a call for "allow once": the command / path / url, else the whole input.
export function permKey(tool: string, input: unknown): string {
  const o = input && typeof input === 'object' ? (input as Record<string, unknown>) : {}
  for (const k of ['command', 'file_path', 'notebook_path', 'url']) {
    if (typeof o[k] === 'string') return `${k}:${(o[k] as string).trim()}`
  }
  try {
    return JSON.stringify(input) ?? ''
  } catch {
    return String(tool)
  }
}

// Asks the phone to decide a permission dialog; undefined = leave it to the terminal dialog
// (answered there, no answer in time, the server is gone, or the turn was interrupted).
async function permFromPhone(
  $: EngineInterface,
  rt: Runtime,
  tool: string,
  input: unknown,
  always: boolean,
  signal: AbortSignal,
): Promise<'allow' | 'always' | 'deny' | undefined> {
  const r = await postTalk($, rt, '/api/talk/perm', { kind: 'ask', tool, input, always })
  const id = typeof r?.id === 'string' ? r.id : undefined
  if (!id) return undefined
  const startedAt = await $.clock.now()
  let polls = Math.ceil(PERM_WAIT_MS / PERM_POLL_MS) + 2
  let failures = 0
  while (!signal.aborted && polls-- > 0 && (await $.clock.now()) - startedAt < PERM_WAIT_MS) {
    try {
      const res = await $.http.fetch(`${rt.baseUrl}/api/talk/perm/${encodeURIComponent(id)}/wait?wait=${PERM_POLL_MS}`)
      if (res.status === 404) return undefined
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
      const w = JSON.parse(res.text) as { state?: unknown; decision?: unknown }
      failures = 0
      if (w.state === 'desk') return undefined
      if (w.state === 'decided' && (w.decision === 'allow' || w.decision === 'always' || w.decision === 'deny')) return w.decision
    } catch {
      if (++failures >= 3) break
    }
  }
  await postJson($, `${rt.baseUrl}/api/talk/perm/${encodeURIComponent(id)}/cancel`, {}, HINT_TIMEOUT_MS)
  return undefined
}

// Compacts now (between turns); a turn that started meanwhile defers it to its end. Reports fresh gauges either way.
async function runCompact($: EngineInterface, rt: Runtime, instructions?: string): Promise<void> {
  try {
    await $.session.compact(instructions ? { instructions } : {})
  } catch {
    if (rt.isTurnRunning) {
      rt.pendingCompact = { instructions }
      return
    }
  }
  try {
    await postGauges($, rt, await $.session.usage())
  } catch {}
}

// Context fill, cost and rate-limit windows to the server (the phone's gauges), in herdr panes only.
async function postGauges($: EngineInterface, rt: Runtime, g: Gauges): Promise<void> {
  if (!rt.inbox) return
  await postJson(
    $,
    `${rt.baseUrl}/api/mod/measure`,
    {
      context: { percent: g.context.percent, tokens: g.context.tokens, window: g.context.window },
      costUsd: g.cost?.usd,
      rateLimits: g.rateLimits.map(r => ({ kind: r.kind, percentUsed: r.percentUsed, resetsAt: r.resetsAt })),
    },
    HINT_TIMEOUT_MS,
  )
}

// Local dev server URLs in `text`, first mention of each port, normalised to http(s)://<host>:<port>/
// (0.0.0.0 and [::] become localhost, which the phone side rewrites to the machine).
export function findDevUrls(text: string): { port: number; url: string }[] {
  const seen = new Set<number>()
  const found: { port: number; url: string }[] = []
  for (const m of text.matchAll(DEV_URL_RE)) {
    const port = Number(m[2])
    if (!Number.isInteger(port) || port < 1 || port > 65535 || seen.has(port)) continue
    seen.add(port)
    const scheme = m[0].toLowerCase().startsWith('https') ? 'https' : 'http'
    const raw = (m[1] ?? 'localhost').toLowerCase()
    const host = raw === '0.0.0.0' || raw.startsWith('[') ? 'localhost' : raw
    found.push({ port, url: `${scheme}://${host}:${port}/` })
  }
  return found
}

function portOf(url: string): number | undefined {
  const m = /^https?:\/\/[^/:]+(?::(\d+))?/i.exec(url)
  if (!m) return undefined
  return m[1] ? Number(m[1]) : url.toLowerCase().startsWith('https') ? 443 : 80
}

let homeCheck: { at: number; isHome: boolean } | undefined

// Whether this session is the pinned home agent (asked of the server, cached briefly).
async function isHome($: EngineInterface, baseUrl: string): Promise<boolean> {
  if (homeCheck && Date.now() - homeCheck.at < HOME_TTL_MS) return homeCheck.isHome
  let isHomeNow = false
  try {
    const [pane, session] = await Promise.all([
      $.env.get('HERDR_PANE_ID').catch(() => undefined),
      $.session.id().catch(() => undefined),
    ])
    const res = await Promise.race([$.http.fetch(`${baseUrl}/api/home`), $.clock.sleep(1_500).then(() => null)])
    if (res && res.ok) {
      const h = JSON.parse(res.text) as { pane?: unknown; session?: unknown }
      isHomeNow = (!!pane && h.pane === pane) || (!!session && h.session === session)
    }
  } catch {}
  homeCheck = { at: Date.now(), isHome: isHomeNow }
  return isHomeNow
}


async function post($: EngineInterface, baseUrl: string, item: Surface): Promise<{ ok: boolean; id?: string; why?: string }> {
  return postJson($, `${baseUrl}/api/surface`, { ...item, tool: item.tool === true }, POST_TIMEOUT_MS)
}

// F3: POST /api/hint (ask / ask_clear). Short timeout, result ignored.
async function hint($: EngineInterface, baseUrl: string, body: Record<string, unknown>): Promise<void> {
  await postJson($, `${baseUrl}/api/hint`, body, HINT_TIMEOUT_MS)
}

// POSTs `body` plus this session's id and herdr pane; never throws.
async function postJson(
  $: EngineInterface,
  url: string,
  body: { session?: string; pane?: string; [k: string]: unknown },
  timeoutMs: number,
): Promise<{ ok: boolean; id?: string; why?: string }> {
  try {
    const [session, pane] = await Promise.all([
      body.session ?? $.session.id().catch(() => undefined),
      $.env.get('HERDR_PANE_ID').catch(() => undefined),
    ])
    const payload = { ...body }
    if (session) payload.session = session
    if (pane) payload.pane = pane
    const timeout = $.clock.sleep(timeoutMs).then(
      () => null,
      () => new Promise<null>(() => {}),
    )
    const res = await Promise.race([
      $.http.fetch(url, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(payload),
      }),
      timeout,
    ])
    if (res === null) return { ok: false, why: 'timed out' }
    if (!res.ok) return { ok: false, why: `HTTP ${res.status}` }
    let id: string | undefined
    try {
      const parsed: unknown = JSON.parse(res.text)
      if (parsed && typeof parsed === 'object' && 'id' in parsed) id = String((parsed as { id: unknown }).id)
    } catch {}
    return { ok: true, id }
  } catch (err) {
    return { ok: false, why: err instanceof Error ? err.message : String(err) }
  }
}

async function postFull(
  $: EngineInterface,
  url: string,
  body: Record<string, unknown>,
  timeoutMs: number,
): Promise<{ ok: boolean; json: Record<string, unknown>; why?: string }> {
  try {
    const [session, pane] = await Promise.all([$.session.id().catch(() => undefined), $.env.get('HERDR_PANE_ID').catch(() => undefined)])
    const payload: Record<string, unknown> = { ...body }
    if (session) payload.session = session
    if (pane) payload.pane = pane
    const timeout = $.clock.sleep(timeoutMs).then(
      () => null,
      () => new Promise<null>(() => {}),
    )
    const res = await Promise.race([
      $.http.fetch(url, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(payload) }),
      timeout,
    ])
    if (res === null) return { ok: false, json: {}, why: 'the sheprd server timed out' }
    let json: Record<string, unknown> = {}
    try {
      const v: unknown = JSON.parse(res.text)
      if (v && typeof v === 'object') json = v as Record<string, unknown>
    } catch {}
    return res.ok ? { ok: true, json } : { ok: false, json, why: `HTTP ${res.status}` }
  } catch (err) {
    return { ok: false, json: {}, why: err instanceof Error ? err.message : String(err) }
  }
}

async function absolute($: EngineInterface, p: string, cwd?: string): Promise<string> {
  if (p.startsWith('/')) return p
  if (p.startsWith('~/')) {
    const home = await $.env.get('HOME')
    return home ? `${home}${p.slice(1)}` : p
  }
  const base = cwd ?? (await $.session.cwd())
  return `${base.replace(/\/+$/, '')}/${p.replace(/^\.\//, '')}`
}

async function isFreshFile($: EngineInterface, path: string, since: number): Promise<boolean> {
  try {
    const st = await $.fs.stat(path)
    return st.kind === 'file' && st.mtimeMs >= since - 2000
  } catch {
    return false
  }
}

// Finds the first image block in a tool result: MCP's { type: 'image', data, mimeType }
// or the Messages API's { type: 'image', source: { data, media_type } }.
function findImage(value: unknown, depth = 0): { data: string; mime: string } | undefined {
  if (depth > 6 || !value || typeof value !== 'object') return undefined
  if (Array.isArray(value)) {
    for (const v of value) {
      const found = findImage(v, depth + 1)
      if (found) return found
    }
    return undefined
  }
  const o = value as Record<string, unknown>
  if (o.type === 'image') {
    if (typeof o.data === 'string' && o.data.length > 100) {
      return { data: o.data, mime: typeof o.mimeType === 'string' ? o.mimeType : 'image/png' }
    }
    const src = o.source as Record<string, unknown> | undefined
    if (src && typeof src.data === 'string' && src.data.length > 100) {
      return { data: src.data, mime: typeof src.media_type === 'string' ? src.media_type : 'image/png' }
    }
  }
  for (const v of Object.values(o)) {
    const found = findImage(v, depth + 1)
    if (found) return found
  }
  return undefined
}

