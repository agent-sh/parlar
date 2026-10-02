// The voice strip above the prompt: what parlard is doing, the last line heard or spoken, and
// buttons for mute, voice, stop and focus. /parlar (or the strip's pane button) opens the voice
// pane: the conversation so far, the sessions to talk to, and the mic and speaker. It follows `parlar ctl watch` and draws nothing while
// the conversation is off or parlard is not running. The command hooks in hooks.json carry the
// conversation itself; this module only shows it.
import { atom, read, update } from 'claude-code'
import type { EngineInterface, Register } from 'claude-code'

import type { Device, HistoryLine, Panel, Phase, Rows, SessionRow, Strip } from '../types'
import { AGENT, registerRows, USER, withNote } from './rows'

const IDLE: Strip = {
  connected: false,
  phase: 'stopped',
  muted: false,
  voiceOff: false,
  focused: false,
  levels: [],
  caption: null,
  flare: false,
}
const strip = atom({ plugin: 'parlar', key: 'strip' } as const, IDLE)
// the rows rows.tsx draws; the strip adds parlar's notes, which only the watch stream carries
const rows = atom({ plugin: 'parlar', key: 'rows' } as const, { wakes: {}, calls: {} } as Rows)
const panel = atom(
  { plugin: 'parlar', key: 'panel' } as const,
  { history: [], sessions: [], devices: null } as Panel,
)

const PANE = 'parlar'
// lines the pane keeps, and devices it lists per kind
const HISTORY = 200
const DEVICES = 6

const BARS = '▁▂▃▄▅▆▇█'
const METER = 8
// level events come at 30 Hz; the terminal redraws at this pace at most
const FRAME_MS = 125
const CAPTION_MS = 15_000
// the longest phase word ('connecting'), so the line does not shift between phases
const WORD_WIDTH = 10
// cells of the strip's fixed parts: '● voice ' and the padded word, then each label and button
// as the terminal draws it ('m: mute'), each with the gap before it
const CELLS = {
  phase: 8 + 10,
  meter: 1 + 8,
  notFocused: 1 + 16,
  captionsOnly: 1 + 13,
  talk: 1 + 12,
  mute: 1 + 9,
  voice: 1 + 12,
  stop: 1 + 7,
  pane: 1 + 7,
}

/** What fits in `width`: mute and stop always (and talk here off focus), the rest by priority. */
export function fits(width: number, s: { focused: boolean; voiceOff: boolean }, metering: boolean) {
  let left = width - CELLS.phase - CELLS.mute - CELLS.stop - (s.focused ? 0 : CELLS.talk)
  const take = (want: boolean, cells: number) => {
    if (!want || left < cells) return false
    left -= cells
    return true
  }
  const meter = take(metering, CELLS.meter)
  const voice = take(true, CELLS.voice)
  const notFocused = take(!s.focused, CELLS.notFocused)
  const captionsOnly = take(s.voiceOff, CELLS.captionsOnly)
  const pane = take(true, CELLS.pane)
  return { meter, voice, notFocused, captionsOnly, pane }
}
const FLARE_MS = 1_200

const WORDS: Record<Phase, string> = {
  stopped: 'off',
  connecting: 'connecting',
  ready: 'ready',
  listening: 'listening',
  interrupting: 'listening',
  working: 'working',
  speaking: 'speaking',
}

type Event =
  | { ui: 'phase'; phase: Phase; mic_muted: boolean; voice_off: boolean }
  | { ui: 'levels'; user: number; agent: number }
  | { ui: 'tool'; ok: boolean; name: string | null }
  | { ui: 'caption'; who: string; text: string; session?: string; partial?: boolean; call?: string }
  | { ui: 'notice'; text: string }

export const meter = (levels: readonly number[]) =>
  levels
    .map(l => BARS[Math.min(BARS.length - 1, Math.floor(Math.sqrt(Math.max(0, l)) * BARS.length))])
    .join('')

// the module's own values: a reload starts them over, the strip itself lives in $.state
const live = {
  // the plugin's launcher, which finds parlar the way the command hooks do (PARLAR_BIN, the user
  // install directories, npm); plain `parlar` from PATH where the launcher is a .cmd (Windows)
  bin: 'parlar',
  watching: false,
  session: '',
  phase: 'stopped' as Phase,
  levels: [] as number[],
  dirty: false,
  captionAt: 0,
  flareAt: 0,
  // set once the watch has seen parlard, so the first connect of a session raises no toast
  seen: false,
}

function set($: EngineInterface, patch: Partial<Strip>) {
  return update($, strip, s => ({ ...s, ...patch }))
}

async function ctl($: EngineInterface, ...args: string[]) {
  await $.process.run([live.bin, 'ctl', ...args], { timeoutMs: 5_000 }).catch(() => undefined)
}

async function refreshFocus($: EngineInterface) {
  const r = await $.process.run([live.bin, 'ctl', 'state'], { timeoutMs: 2_000 }).catch(() => undefined)
  if (!r || r.exitCode !== 0) return
  try {
    const st = JSON.parse(r.stdout) as { sessions?: Partial<SessionRow>[] }
    const sessions = (st.sessions ?? []).flatMap(s =>
      s.session
        ? [{ session: s.session, cwd: s.cwd ?? '', harness: s.harness ?? '', focused: s.focused === true }]
        : [],
    )
    const focused = sessions.some(s => s.session === live.session && s.focused)
    const was = await read($, strip)
    if (focused !== was.focused) {
      await set($, { focused })
      // typing elsewhere moves the voice without a sound here; say where it went
      const to = sessions.find(s => s.focused)
      if (was.focused && to && was.phase !== 'stopped') $.ui.toast(`Voice moved to ${folder(to.cwd)}`)
    }
    if (JSON.stringify(sessions) !== JSON.stringify((await read($, panel)).sessions)) {
      await update($, panel, p => ({ ...p, sessions }))
    }
  } catch {
    // a daemon mid-restart can answer half a line; the next poll reads it again
  }
}

async function apply($: EngineInterface, ev: Event, now: number) {
  switch (ev.ui) {
    case 'phase': {
      const back = live.seen && !(await read($, strip)).connected && ev.phase !== 'stopped'
      live.seen = true
      if (back) $.ui.toast('parlard is back')
      live.phase = ev.phase
      live.levels = []
      await set($, {
        connected: true,
        phase: ev.phase,
        muted: ev.mic_muted,
        voiceOff: ev.voice_off,
        levels: [],
      })
      void refreshFocus($).catch(quiet)
      return
    }
    case 'levels': {
      const phase = live.phase
      const l =
        phase === 'speaking' ? ev.agent : phase === 'listening' || phase === 'interrupting' ? ev.user : 0
      live.levels = [...live.levels, l].slice(-METER)
      live.dirty = true
      return
    }
    case 'tool':
      if (!ev.ok) {
        live.flareAt = now
        await set($, { flare: true })
      }
      return
    case 'notice':
      $.ui.toast(ev.text, { timeoutMs: 12_000 })
      return
    case 'caption': {
      // a caption names its session; a partial, parlar's own line, or a daemon from before that
      // field belongs to whoever has focus
      const mine = ev.session !== undefined ? ev.session === live.session : (await read($, strip)).focused
      if (!mine) return
      live.captionAt = now
      await set($, { caption: { who: ev.who, text: ev.text } })
      // parlar's answer during a long tool call is drawn on that call's row
      if (ev.call) {
        const call = ev.call
        await update($, rows, r => withNote(r, call, ev.text))
      }
      // the pane keeps finished lines, not the words so far
      if (ev.partial) return
      const line: HistoryLine = { who: ev.who === 'user' ? 'you' : 'parlar', text: ev.text }
      await update($, panel, p => ({ ...p, history: [...p.history, line].slice(-HISTORY) }))
      return
    }
  }
}

async function watch($: EngineInterface) {
  live.watching = true
  let buf = ''
  try {
    for await (const { stream, text } of $.process.spawn({ argv: [live.bin, 'ctl', 'watch'] })) {
      if (stream !== 'stdout') continue
      buf += text
      let nl: number
      while ((nl = buf.indexOf('\n')) >= 0) {
        const line = buf.slice(0, nl)
        buf = buf.slice(nl + 1)
        try {
          await apply($, JSON.parse(line) as Event, await $.clock.now())
        } catch {
          // an event kind this build does not know yet
        }
      }
    }
  } catch {
    // parlar is not on PATH: stay quiet, as the command hooks do
  } finally {
    live.watching = false
    const was = await read($, strip)
    await set($, { connected: false, levels: [] })
    // the watch also ends when the module unloads; only a parlard that no longer answers is news
    if (was.connected && was.phase !== 'stopped' && !(await answers($))) {
      $.ui.toast('parlard stopped: voice is off until it is back', { timeoutMs: 8_000 })
    }
  }
}

async function answers($: EngineInterface) {
  const r = await $.process.run([live.bin, 'ctl', 'state'], { timeoutMs: 2_000 }).catch(() => undefined)
  return r?.exitCode === 0
}

// a timer that fires as the module unloads finds $ gone; the next load starts its own
function quiet() {}

async function frame($: EngineInterface) {
  const now = await $.clock.now()
  const s = await read($, strip)
  const patch: Partial<Strip> = {}
  if (live.dirty) {
    live.dirty = false
    patch.levels = live.levels
  }
  if (s.caption && now - live.captionAt > CAPTION_MS) patch.caption = null
  if (s.flare && now - live.flareAt > FLARE_MS) patch.flare = false
  if (Object.keys(patch).length > 0) await set($, patch)
}

async function refreshDevices($: EngineInterface) {
  const r = await $.process.run([live.bin, 'ctl', 'devices'], { timeoutMs: 5_000 }).catch(() => undefined)
  if (!r || r.exitCode !== 0) return
  try {
    const d = JSON.parse(r.stdout) as { inputs?: Device[]; outputs?: Device[] }
    await update($, panel, p => ({ ...p, devices: { inputs: d.inputs ?? [], outputs: d.outputs ?? [] } }))
  } catch {
    // an answer this build cannot read leaves the pickers as they were
  }
}

async function openPane($: EngineInterface) {
  const placed = await $.ui.open({ id: PANE, title: 'Voice' })
  await Promise.all([refreshFocus($), refreshDevices($)])
  return placed
}

async function pickDevice($: EngineInterface, kind: 'input' | 'output', id: string) {
  await ctl($, kind, id)
  await refreshDevices($)
}

function folder(cwd: string) {
  return cwd.split(/[\\/]/).filter(Boolean).pop() ?? cwd
}

async function pollFocus($: EngineInterface) {
  const s = await read($, strip)
  if (s.connected && s.phase !== 'stopped') await refreshFocus($)
}

export const register: Register = on => {
  registerRows(on)
  on('session.start', async ($, e, next) => {
    const started = await next(e)
    if (!e.isInteractive) return started
    // the hooks started from now on leave the heard and spoken lines to rows.tsx
    await $.env.set('PARLAR_ROWS', '1')
    live.session = await $.session.id()
    const root = $.plugin.root
    live.bin = root.includes('\\') ? 'parlar' : `${root}/bin/parlar`
    await update($, strip, () => IDLE)
    void watch($).catch(quiet)
    // reconnect after parlard starts or restarts
    $.clock.every(5_000, () => {
      if (!live.watching) void watch($).catch(quiet)
    })
    $.clock.every(FRAME_MS, () => void frame($).catch(quiet))
    // focus moves with typing in another session, which sends no event here
    $.clock.every(2_000, () => void pollFocus($).catch(quiet))
    await $.command.register({
      name: 'parlar',
      description: 'Open the voice pane: the conversation, the sessions, the mic and speaker',
      immediate: true,
    })
    return started
  })

  on('command.run', { command: 'parlar' }, async $ => {
    const { isPlaced } = await openPane($)
    return { text: isPlaced ? 'Voice pane opened.' : 'The voice pane will open when there is room.' }
  })

  on('ui.render', { component: 'Pane', requestId: PANE }, async ($, e) => {
    const { Box, Button, Text } = $.ui.resolve(e)
    const s = await read($, strip)
    const p = await read($, panel)
    const room = Math.max(3, (e.viewport?.rows ?? 24) - 12 - p.sessions.length)
    const others = p.sessions.filter(x => x.session !== live.session)
    const here = p.sessions.find(x => x.session === live.session)
    const word = !s.connected ? 'parlard is not running' : s.muted ? 'mic muted' : WORDS[s.phase]
    // one Button per device, so every surface can pick (Select is not on all of them). Plain ALSA
    // lists one card many times: one entry per name, the current one first, at most DEVICES
    const devices = (kind: 'input' | 'output', title: string, list: Device[]) => {
      const named = list.filter((d, i) => d.current || list.findIndex(x => x.name === d.name) === i)
      const shown = [...named.filter(d => d.current), ...named.filter(d => !d.current)].slice(0, DEVICES)
      return (
        <Box flexDirection="column">
          <Text bold>{title}</Text>
          {shown.map(d =>
            d.current ? (
              <Text>● {d.name}</Text>
            ) : (
              <Button
                key={`${kind}-${d.id}`}
                label={`○ ${d.name}`}
                plain
                onPress={() => pickDevice($, kind, d.id)}
              />
            ),
          )}
          {named.length > shown.length && (
            <Text dimColor>
              {named.length - shown.length} more: parlar ctl devices, then parlar ctl {kind} {'<id>'}
            </Text>
          )}
        </Box>
      )
    }
    return (
      <Box flexDirection="column" gap={1}>
        <Text bold color={s.focused ? USER : undefined}>
          {s.focused ? '●' : '○'} voice {word}
          {s.connected && !s.focused ? ', not focused here' : ''}
        </Text>
        <Box flexDirection="column">
          <Text bold>Conversation</Text>
          {p.history.length === 0 && <Text dimColor>Nothing said yet.</Text>}
          {p.history.slice(-room).map(l => (
            <Text color={l.who === 'you' ? USER : AGENT} wrap="wrap">
              {l.who} ▸ {l.text}
            </Text>
          ))}
        </Box>
        <Box flexDirection="column">
          <Text bold>Sessions</Text>
          {here && (
            <Text>
              {here.focused ? '●' : '○'} {folder(here.cwd)} (this one)
            </Text>
          )}
          {s.connected && !here?.focused && (
            // talk attaches this session if parlard does not know it yet, then focuses it
            <Button
              key="attach"
              label="talk here"
              onPress={() => ctl($, 'talk', '--session', live.session)}
            />
          )}
          {others.map(o => (
            <Box flexDirection="row" gap={1}>
              <Text dimColor={!o.focused}>
                {o.focused ? '●' : '○'} {folder(o.cwd)} {o.harness}
              </Text>
              {!o.focused && (
                <Button
                  key={`focus-${o.session}`}
                  label="talk there"
                  plain
                  onPress={() => ctl($, 'focus', o.session)}
                />
              )}
            </Box>
          ))}
        </Box>
        {p.devices && devices('input', 'Mic', p.devices.inputs)}
        {p.devices && devices('output', 'Speaker', p.devices.outputs)}
        <Box flexDirection="row" gap={1}>
          <Button
            key="pane-mute"
            label={s.muted ? 'unmute' : 'mute'}
            onPress={() => ctl($, s.muted ? 'unmute' : 'mute')}
          />
          <Button
            key="pane-voice"
            label={s.voiceOff ? 'voice on' : 'voice off'}
            onPress={() => ctl($, s.voiceOff ? 'voice-on' : 'voice-off')}
          />
          <Button
            key="pane-power"
            label={s.connected && s.phase !== 'stopped' ? 'stop' : 'start'}
            onPress={() => ctl($, s.connected && s.phase !== 'stopped' ? 'off' : 'on')}
          />
        </Box>
      </Box>
    )
  })

  on('prompt.submit', async ($, e, next) => {
    // typing here takes focus; show it without waiting for the poll
    const r = await next(e)
    void refreshFocus($).catch(quiet)
    return r
  })

  on('ui.render', { component: 'AbovePrompt' }, async ($, e, next) => {
    const s = await read($, strip)
    if (e.props.hasSurvey || !s.connected || s.phase === 'stopped') return next(e)
    const { Box, Button, Text } = $.ui.resolve(e)
    // whatever else the band holds stays, under the strip
    const below = await next(e)

    const talking = s.phase === 'speaking' ? AGENT : USER
    const color = s.flare ? 'red' : s.muted ? undefined : s.phase === 'working' ? AGENT : talking
    const word = s.muted ? 'mic muted' : WORDS[s.phase]
    const metering = s.phase === 'listening' || s.phase === 'interrupting' || s.phase === 'speaking'
    const who = s.caption?.who === 'user' ? 'you' : s.caption?.who === 'parlar' ? 'parlar' : 'agent'
    // the engine draws its collapse mark in the last columns
    const width = Math.max(20, e.props.bodyColumns - 4)
    const fit = fits(width, s, metering)

    return (
      <Box flexDirection="column">
        <Box flexDirection="row" gap={1} width={width}>
          {/* fixed widths and no wrapping: the strip stays one line, and its parts do not shift
              as the phase and the meter change */}
          <Box flexShrink={0}>
            <Text color={color} dimColor={s.muted || !s.focused} bold={s.focused} wrap="truncate-end">
              {s.focused ? '●' : '○'} voice {word.padEnd(WORD_WIDTH)}
            </Text>
          </Box>
          {fit.meter && (
            <Box flexShrink={0}>
              <Text color={talking} wrap="truncate-end">
                {meter(s.levels).padEnd(METER)}
              </Text>
            </Box>
          )}
          {fit.notFocused && (
            <Box flexShrink={0}>
              <Text dimColor wrap="truncate-end">
                not focused here
              </Text>
            </Box>
          )}
          {fit.captionsOnly && (
            <Box flexShrink={0}>
              <Text dimColor wrap="truncate-end">
                captions only
              </Text>
            </Box>
          )}
          {/* the caption takes what is left; a long one keeps its last words */}
          <Box flexGrow={1} flexShrink={1} minWidth={0}>
            {s.focused && s.caption && (
              <Text dimColor wrap="truncate-start">
                {who}: {s.caption.text}
              </Text>
            )}
          </Box>
          <Box flexShrink={0} gap={1}>
            {!s.focused && (
              // talk attaches a session that started before parlard, which focus alone cannot
              <Button
                key="focus"
                label="talk here"
                hotkey="t"
                plain
                onPress={() => ctl($, 'talk', '--session', live.session)}
              />
            )}
            <Button
              key="mute"
              label={s.muted ? 'unmute' : 'mute'}
              hotkey="m"
              plain
              onPress={() => ctl($, s.muted ? 'unmute' : 'mute')}
            />
            {fit.voice && (
              <Button
                key="voice"
                label={s.voiceOff ? 'voice on' : 'voice off'}
                hotkey="v"
                plain
                onPress={() => ctl($, s.voiceOff ? 'voice-on' : 'voice-off')}
              />
            )}
            <Button key="stop" label="stop" hotkey="s" plain onPress={() => ctl($, 'off')} />
            {fit.pane && <Button key="pane" label="pane" hotkey="p" plain onPress={() => openPane($)} />}
          </Box>
        </Box>
        {below}
      </Box>
    )
  })
}
