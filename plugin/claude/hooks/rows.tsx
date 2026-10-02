// The spoken exchange in the transcript: each say call draws as a "parlar" line where the call
// sits, and each utterance as a "you" line on the row that delivered it (the voice wake row, or
// the tool call whose hook passed it on). Without this module the command hooks print the same
// lines as hook messages at the next tool call; PARLAR_ROWS, set in strip.tsx's session.start, tells
// them to leave those lines here.
// ctrl+o shows the wake rows and say groups as the engine draws them.
import { atom, read, update } from 'claude-code'
import type { EngineInterface, On } from 'claude-code'

import type { Rows } from '../types'

export const USER = '#5fafff'
export const AGENT = '#ffaf00'

const SAY = 'mcp__plugin_parlar_parlar__say'
// what format::utterances and the wake message in hooks.json write
const WAKE = 'The user spoke to you by voice:'
const ACTIVATED = 'Voice mode is on.'
// how mcp.rs starts a say result that was not spoken
const NOT_SPOKEN = ['Not spoken', 'Voice mode is off']
// what mcp.rs and hook.rs put ahead of utterances in a tool result: any other tool's output is
// the tool's own, even when it quotes a delivery
const RESULT_CUES = ['The user said meanwhile:', 'The user asked you to stop']
// rows kept per session; older ones fall back to the engine's drawing
const KEEP = 300

const rows = atom({ plugin: 'parlar', key: 'rows' } as const, { wakes: {}, calls: {} } as Rows)

// the tool call a later PostToolUse context row belongs to: the last result appended
const last = { call: '' }

/** The utterances a delivery carries, cleaned of the reminder and the notes around them. */
export function heardIn(text: string): string[] {
  const out: string[] = []
  for (const m of text.matchAll(/\[voice u\d+(?: continues u\d+)?\] ([\s\S]*?)(?=\n\[voice u|\n\(|$)/g)) {
    const lines = (m[1] ?? '').split('\n').filter(l => !l.startsWith(ACTIVATED))
    const said = lines.join(' ').trim()
    if (said) out.push(said)
  }
  return out
}

type Block = { type: string; text?: string; tool_use_id?: string; content?: string | Block[] }

function textOf(content: string | readonly Block[] | undefined): string {
  if (typeof content === 'string') return content
  return (content ?? [])
    .map(b => (b.type === 'text' ? (b.text ?? '') : ''))
    .filter(Boolean)
    .join('\n')
}

function keep(map: Record<string, string[]>, key: string, heard: string[]) {
  const next = { ...map, [key]: [...(map[key] ?? []), ...heard] }
  const keys = Object.keys(next)
  for (const k of keys.slice(0, Math.max(0, keys.length - KEEP))) delete next[k]
  return next
}

function remember($: EngineInterface, kind: keyof Rows, key: string, heard: string[]) {
  return update($, rows, r => ({ ...r, [kind]: keep(r[kind], key, heard) }))
}

function sayText(input: unknown): string | undefined {
  const t = (input as { text?: unknown } | null)?.text
  return typeof t === 'string' && t.trim() ? t.trim() : undefined
}

function outputText(output: unknown): string {
  return Array.isArray(output) ? textOf(output as Block[]) : typeof output === 'string' ? output : ''
}

/** Why a say was not spoken, from its result: the first sentence of mcp.rs's note. */
function notSpoken(output: unknown): string | undefined {
  const t = outputText(output)
  if (!NOT_SPOKEN.some(n => t.startsWith(n))) return undefined
  return t.split(/(?<=\.) /)[0]?.replace(/\.$/, '')
}

/** The utterances a tool result passed on, after the cue parlar writes ahead of them. */
function heardInResult(text: string): string[] {
  const cue = RESULT_CUES.map(c => text.indexOf(c)).find(i => i >= 0)
  return cue === undefined ? [] : heardIn(text.slice(cue))
}

type Call = { tool_use_id?: string; tool: string; input: unknown; output?: unknown }
type Line = { who: 'you' | 'parlar'; text: string; note?: string }

/** The lines one tool call adds: what it spoke, then what the user said that it passed on. */
function linesOf(c: Call, r: Rows): Line[] {
  const out: Line[] = []
  if (c.tool === SAY) {
    const t = sayText(c.input)
    if (t) out.push({ who: 'parlar', text: t, note: notSpoken(c.output) })
  }
  // a say result or a refused call's reason carries them in the row's own output; a PostToolUse
  // context row only in what session.append noted
  const heard = [...heardInResult(outputText(c.output)), ...(r.calls[c.tool_use_id ?? ''] ?? [])]
  for (const h of heard) out.push({ who: 'you', text: h })
  return out
}

type Appended = { door: string; origin: { kind: string }; uuid: string; message: unknown; agentId?: string }
type Note = { kind: keyof Rows; key: string; heard: string[] }

/** What one appended row tells about which row delivered which utterances. */
export function notesOf(e: Appended, lastCall: string): { notes: Note[]; lastCall: string } {
  const m = e.message as { name?: string; content?: string | Block[] }
  const notes: Note[] = []
  // speech goes to the main thread; a subagent's rows would move lastCall off it
  if (e.agentId) return { notes, lastCall }
  if (e.door === 'prompt' && e.origin.kind === 'task-notification') {
    const text = textOf(m.content)
    if (text.includes(WAKE)) notes.push({ kind: 'wakes', key: e.uuid, heard: heardIn(text.slice(text.indexOf(WAKE))) })
  }
  // a tool result's own utterances are read from the row's output; here it only marks the call
  // a following PostToolUse context row belongs to
  if (e.door === 'tool-result' && Array.isArray(m.content)) {
    for (const b of m.content) if (b.type === 'tool_result' && b.tool_use_id) lastCall = b.tool_use_id
  }
  if (e.door === 'hook-context' && m.name === 'hook_additional_context') {
    notes.push({ kind: 'calls', key: lastCall, heard: heardIn(textOf(m.content)) })
  }
  return { notes: notes.filter(n => n.key && n.heard.length > 0), lastCall }
}

export function registerRows(on: On) {
  on('session.append', async ($, e, next) => {
    const { notes, lastCall } = notesOf(e, last.call)
    last.call = lastCall
    for (const n of notes) await remember($, n.kind, n.key, n.heard)
    return next(e)
  })

  on('ui.render', { component: 'UserMessage' }, async ($, e, next) => {
    if (e.props.isExpanded || e.props.origin.kind !== 'task-notification') return next(e)
    const heard = (await read($, rows)).wakes[e.requestId ?? '']
    if (!heard) return next(e)
    const { Box, Text } = $.ui.resolve(e)
    return (
      <Box flexDirection="column">
        {heard.map(said => (
          <Text color={USER}>you ▸ {said}</Text>
        ))}
      </Box>
    )
  })

  on('ui.render', { component: 'ToolGroup' }, async ($, e, next) => {
    if (e.props.isExpanded) return next(e)
    const r = await read($, rows)
    const lines = e.props.calls.flatMap(c => linesOf(c, r))
    if (lines.length === 0) return next(e)
    const { Box, Text } = $.ui.resolve(e)
    // a group of say calls alone is the lines; any other call keeps the engine's summary above
    const onlySay = e.props.calls.every(c => c.tool === SAY)
    const above = onlySay ? null : await next(e)
    return (
      <Box flexDirection="column">
        {above}
        {lines.map(l => (
          <Text color={l.who === 'you' ? USER : AGENT} dimColor={l.note !== undefined}>
            {l.who} ▸ {l.text}
            {l.note ? ` (${l.note})` : ''}
          </Text>
        ))}
      </Box>
    )
  })

  on('ui.render', { component: 'ToolUse' }, async ($, e, next) => {
    const lines = linesOf(e.props, await read($, rows))
    if (lines.length === 0) return next(e)
    const { Box, Text } = $.ui.resolve(e)
    const above = e.props.tool === SAY ? null : await next(e)
    return (
      <Box flexDirection="column">
        {above}
        {lines.map(l => (
          <Text color={l.who === 'you' ? USER : AGENT} dimColor={l.note !== undefined}>
            {l.who} ▸ {l.text}
            {l.note ? ` (${l.note})` : ''}
          </Text>
        ))}
      </Box>
    )
  })

  // a say call's result is "Said." plus what it passed on, which its row already shows
  on('ui.render', { component: 'ToolResult' }, async ($, e, next) => {
    if (e.props.tool !== SAY || e.props.isErrored) return next(e)
    const { Box } = $.ui.resolve(e)
    return <Box />
  })
}
