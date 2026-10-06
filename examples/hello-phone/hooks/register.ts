import type { Register } from 'claude-code'

// The smallest useful sheprd mod: `/hello-phone [text]` puts a card on your phone (and a push, if it's away).
// Copy this folder to start your own. The server API is in docs/API.md ("Surfacing"); the same endpoint takes
// images, links and quick-reply buttons.
//
//   claude plugin marketplace add ./examples   # once, from the sheprd repo
//   claude plugin install hello-phone@sheprd-examples --scope user

export const register: Register = (on, options) => {
  const baseUrl = String(options.baseUrl ?? 'http://127.0.0.1:7878').replace(/\/+$/, '')

  on('session.start', async ($, e, next) => {
    await $.command
      .register({ name: 'hello-phone', description: 'Send a hello card to your phone (sheprd)', argumentHint: '[text]', immediate: true })
      .catch(() => undefined)
    return next(e)
  })

  on('command.run', { command: 'hello-phone' }, async ($, e) => {
    // herdr sets HERDR_PANE_ID: with it the card belongs to this agent (tap = its chat)
    const [session, pane] = await Promise.all([$.session.id().catch(() => undefined), $.env.get('HERDR_PANE_ID').catch(() => undefined)])
    const text = String(e.args ?? '').trim() || 'Hello from your agent 👋'
    try {
      const res = await $.http.fetch(`${baseUrl}/api/surface`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ session, pane, tool: false, level: 'info', title: 'Hello', body: text, replies: ['👍'] }),
      })
      return { text: res.ok ? 'Sent to your phone.' : `sheprd answered ${res.status}` }
    } catch {
      return { text: `sheprd isn't reachable at ${baseUrl} (is it running? try \`sheprd doctor\`)` }
    }
  })
}
