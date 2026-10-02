// The voice strip above the prompt: what parlard is doing, the last line heard or spoken, and
// buttons for mute, voice, stop and focus. It follows `parlar ctl watch` and draws nothing while
// the conversation is off or parlard is not running. The command hooks in hooks.json carry the
// conversation itself; this module only shows it.
import { atom, read, update } from 'claude-code'
import type { EngineInterface, Register } from 'claude-code'

import type { Phase, Strip } from '../types'

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

const USER = '#5fafff'
const AGENT = '#ffaf00'
const BARS = '▁▂▃▄▅▆▇█'
const METER = 8
// level events come at 30 Hz; the terminal redraws at this pace at most
const FRAME_MS = 125
const CAPTION_MS = 15_000
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
  | { ui: 'caption'; who: string; text: string }

export const meter = (levels: readonly number[]) =>
  levels
    .map(l => BARS[Math.min(BARS.length - 1, Math.floor(Math.sqrt(Math.max(0, l)) * BARS.length))])
    .join('')

// the module's own values: a reload starts them over, the strip itself lives in $.state
const live = {
  watching: false,
  session: '',
  phase: 'stopped' as Phase,
  levels: [] as number[],
  dirty: false,
  captionAt: 0,
  flareAt: 0,
}

function set($: EngineInterface, patch: Partial<Strip>) {
  return update($, strip, s => ({ ...s, ...patch }))
}

async function ctl($: EngineInterface, ...args: string[]) {
  await $.process.run(['parlar', 'ctl', ...args], { timeoutMs: 5_000 }).catch(() => undefined)
}

async function refreshFocus($: EngineInterface) {
  const r = await $.process.run(['parlar', 'ctl', 'state'], { timeoutMs: 2_000 }).catch(() => undefined)
  if (!r || r.exitCode !== 0) return
  try {
    const st = JSON.parse(r.stdout) as { sessions?: { session?: string; focused: boolean }[] }
    const focused = st.sessions?.some(s => s.session === live.session && s.focused) ?? false
    if (focused !== (await read($, strip)).focused) await set($, { focused })
  } catch {
    // a daemon mid-restart can answer half a line; the next poll reads it again
  }
}

async function apply($: EngineInterface, ev: Event, now: number) {
  switch (ev.ui) {
    case 'phase':
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
    case 'caption':
      live.captionAt = now
      await set($, { caption: { who: ev.who, text: ev.text } })
      return
  }
}

async function watch($: EngineInterface) {
  live.watching = true
  let buf = ''
  try {
    for await (const { stream, text } of $.process.spawn({ argv: ['parlar', 'ctl', 'watch'] })) {
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
    await set($, { connected: false, levels: [] })
  }
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

async function pollFocus($: EngineInterface) {
  const s = await read($, strip)
  if (s.connected && s.phase !== 'stopped') await refreshFocus($)
}

export const register: Register = on => {
  on('session.start', async ($, e, next) => {
    const started = await next(e)
    if (!e.isInteractive) return started
    live.session = await $.session.id()
    await update($, strip, () => IDLE)
    void watch($).catch(quiet)
    // reconnect after parlard starts or restarts
    $.clock.every(5_000, () => {
      if (!live.watching) void watch($).catch(quiet)
    })
    $.clock.every(FRAME_MS, () => void frame($).catch(quiet))
    // focus moves with typing in another session, which sends no event here
    $.clock.every(2_000, () => void pollFocus($).catch(quiet))
    return started
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
    const who = s.caption?.who === 'user' ? 'you' : 'agent'

    return (
      <Box flexDirection="column">
        {/* the engine draws its collapse mark in the last columns */}
        <Box flexDirection="row" gap={1} width={Math.max(20, e.props.bodyColumns - 4)}>
          <Text color={color} dimColor={s.muted || !s.focused} bold={s.focused}>
            {s.focused ? '●' : '○'} voice {word}
          </Text>
          {metering && s.levels.length > 0 && <Text color={talking}>{meter(s.levels)}</Text>}
          {!s.focused && <Text dimColor>not focused here</Text>}
          {s.voiceOff && <Text dimColor>captions only</Text>}
          {s.focused && s.caption && (
            <Box flexGrow={1} flexShrink={1}>
              <Text dimColor wrap="truncate-end">
                {who}: {s.caption.text}
              </Text>
            </Box>
          )}
          {!s.focused && (
            <Button
              key="focus"
              label="talk here"
              hotkey="t"
              plain
              onPress={() => ctl($, 'focus', live.session)}
            />
          )}
          <Button
            key="mute"
            label={s.muted ? 'unmute' : 'mute'}
            hotkey="m"
            plain
            onPress={() => ctl($, s.muted ? 'unmute' : 'mute')}
          />
          <Button
            key="voice"
            label={s.voiceOff ? 'voice on' : 'voice off'}
            hotkey="v"
            plain
            onPress={() => ctl($, s.voiceOff ? 'voice-on' : 'voice-off')}
          />
          <Button key="stop" label="stop" hotkey="s" plain onPress={() => ctl($, 'off')} />
        </Box>
        {below}
      </Box>
    )
  })
}
