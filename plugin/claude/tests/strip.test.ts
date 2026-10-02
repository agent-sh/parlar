import { expect, mock, test } from 'claude-code/testing'
import type { On } from 'claude-code'

import { fits } from '../hooks/strip'

const SESSION = 'abc-123'
const BAND = { component: 'AbovePrompt', props: { hasSurvey: false, isWorking: false, maxRows: 10, bodyColumns: 120, scroll: { offset: 0, bodyRows: 9 }, view: {} } } as const

// a parlard that sends `events` on the watch stream, then holds it open
function daemon(on: On, events: object[], focusedAtStart: boolean) {
  const ran: string[][] = []
  const toasts: string[] = []
  // what the test changes as it goes: where focus is, and whether parlard still answers
  const now = { focused: focusedAtStart, down: false }
  // each watch holds its stream open until released, as a live parlard does
  let release = () => {}
  on('session.start', ($, e) => ({ cwd: e.cwd }))
  on('ui.render', ($, e) => $.ui.resolve(e).Box({}))
  on('ui.toast', async ($, e) => {
    toasts.push(e.text)
    return { value: undefined }
  })
  on('session.id', async () => ({ value: SESSION }))
  on('env.set', async () => ({ value: undefined }))
  on('command.register', async () => ({ value: { command: 'parlar' } }))
  on('ui.open', async () => ({ value: { isPlaced: true } }))
  on('process.spawn', async function* () {
    const held = new Promise<void>(r => (release = r))
    for (const ev of events) yield { stream: 'stdout' as const, text: JSON.stringify(ev) + '\n' }
    await held
    return { value: { code: 0, signal: null } }
  })
  on('process.run', ($, e) => {
    ran.push([...e.argv])
    const focused = now.focused
    if (now.down) return { value: { exitCode: 1, stdout: '', stderr: 'not running', isStdoutTruncated: false, isStderrTruncated: false } }
    const state = {
      sessions: [
        { session: SESSION, cwd: '/w/parlar', harness: 'claude', focused },
        { session: 'other-1', cwd: '/w/ginza', harness: 'codex', focused: !focused },
      ],
    }
    const devices = {
      inputs: [
        { id: 'in-default', name: 'System default', current: true },
        { id: 'in-headset', name: 'Headset', current: false },
      ],
      outputs: [{ id: 'out-default', name: 'System default', current: true }],
    }
    const answers: Record<string, object> = { state, devices }
    const stdout = JSON.stringify(answers[e.argv[2] ?? ''] ?? { kind: 'ok' })
    return { value: { exitCode: 0, stdout, stderr: '', isStdoutTruncated: false, isStderrTruncated: false } }
  })
  return { ran, release: () => release(), toasts, now }
}

for (const surface of ['terminal', 'desktop'] as const) {
  test(`strip shows the phase, the caption and acts on ${surface}`, async ($, on) => {
    const clock = mock.clock(on, { now: 1_000 })
    const d = daemon(on, [
      { ui: 'phase', phase: 'listening', mic_muted: false, voice_off: false },
      { ui: 'caption', who: 'user', text: 'open the router file' },
    ], true)
    await $.session.start({ cwd: '/repo', surface, isInteractive: true })
    await clock.advance(2_000)
    const ui = await $.ui.mount({ plugin: 'parlar', surface, ...BAND })
    expect(await ui.find({ type: 'Text', text: /voice listening/ })).toBeDefined()
    expect(await ui.find({ type: 'Text', text: /you: open the router file/ })).toBeDefined()
    expect(await ui.find({ key: 'focus' })).toBeUndefined()
    await ui.press({ key: 'mute' })
    expect(d.ran.map(a => a.slice(1))).toContainEqual(['ctl', 'mute'])
    expect(d.ran[0]?.[0]).toMatch(/\/bin\/parlar$/)
    await ui.unmount()
    d.release()
  })
}

test('strip offers focus when another session has it', async ($, on) => {
  const clock = mock.clock(on, { now: 1_000 })
  const d = daemon(on, [{ ui: 'phase', phase: 'listening', mic_muted: false, voice_off: false }], false)
  await $.session.start({ cwd: '/repo', surface: 'terminal', isInteractive: true })
  await clock.advance(2_000)
  const ui = await $.ui.mount({ plugin: 'parlar', surface: 'terminal', ...BAND })
  expect(await ui.find({ type: 'Text', text: /not focused here/ })).toBeDefined()
  await ui.press({ key: 'focus' })
  expect(d.ran.map(a => a.slice(1))).toContainEqual(['ctl', 'talk', '--session', SESSION])
  await ui.unmount()
  d.release()
})

test('strip draws nothing while the conversation is off', async ($, on) => {
  const clock = mock.clock(on, { now: 1_000 })
  const d = daemon(on, [{ ui: 'phase', phase: 'stopped', mic_muted: false, voice_off: false }], true)
  await $.session.start({ cwd: '/repo', surface: 'terminal', isInteractive: true })
  await clock.advance(2_000)
  const ui = await $.ui.mount({ plugin: 'parlar', surface: 'terminal', ...BAND })
  expect(await ui.find({ key: 'stop' })).toBeUndefined()
  await ui.unmount()
  d.release()
})

const PANE = { component: 'Pane', requestId: 'parlar', props: {} } as const

for (const surface of ['terminal', 'desktop'] as const) {
  test(`the pane lists the conversation, sessions and devices on ${surface}`, async ($, on) => {
    const clock = mock.clock(on, { now: 1_000 })
    const d = daemon(
      on,
      [
        { ui: 'phase', phase: 'ready', mic_muted: false, voice_off: false },
        { ui: 'caption', who: 'user', text: 'open the router', session: 'abc-123' },
        { ui: 'caption', who: 'user', text: 'open the', partial: true },
        { ui: 'caption', who: 'agent', text: 'Another session speaking.', session: 'other-1' },
      ],
      true,
    )
    await $.session.start({ cwd: '/w/parlar', surface, isInteractive: true })
    await clock.advance(2_000)
    const r = await $.command.run({ command: 'parlar' } as never)
    expect(String((r as { text?: string }).text)).toMatch(/opened/)
    const ui = await $.ui.mount({ plugin: 'parlar', surface, ...PANE } as never)
    expect(await ui.find({ type: 'Text', text: /parlar \(this one\)/ })).toBeDefined()
    expect(await ui.find({ type: 'Text', text: /you ▸ open the router/ })).toBeDefined()
    // a partial is not kept, and another session's line is not this conversation
    expect(await ui.findAll({ type: 'Text', text: /you ▸/ })).toHaveLength(1)
    expect(await ui.find({ type: 'Text', text: /Another session/ })).toBeUndefined()
    expect(await ui.find({ key: 'attach' })).toBeUndefined()
    expect(await ui.find({ type: 'Text', text: /ginza codex/ })).toBeDefined()
    expect(await ui.find({ type: 'Text', text: /● System default/ })).toBeDefined()
    await ui.press({ key: 'focus-other-1' })
    expect(d.ran.map(a => a.slice(1))).toContainEqual(['ctl', 'focus', 'other-1'])
    await ui.press({ key: 'input-in-headset' })
    expect(d.ran.map(a => a.slice(1))).toContainEqual(['ctl', 'input', 'in-headset'])
    await ui.unmount()
    d.release()
  })
}

test('the pane offers talk here when this session lost focus', async ($, on) => {
  const clock = mock.clock(on, { now: 1_000 })
  const d = daemon(on, [{ ui: 'phase', phase: 'ready', mic_muted: false, voice_off: false }], false)
  await $.session.start({ cwd: '/w/parlar', surface: 'terminal', isInteractive: true })
  await clock.advance(2_000)
  await $.command.run({ command: 'parlar' } as never)
  const ui = await $.ui.mount({ plugin: 'parlar', surface: 'terminal', ...PANE } as never)
  await ui.press({ key: 'attach' })
  expect(d.ran.map(a => a.slice(1))).toContainEqual(['ctl', 'talk', '--session', SESSION])
  await ui.unmount()
  d.release()
})

test('toasts say where the voice went and when parlard stops', async ($, on) => {
  const clock = mock.clock(on, { now: 1_000 })
  const d = daemon(on, [{ ui: 'phase', phase: 'ready', mic_muted: false, voice_off: false }], true)
  await $.session.start({ cwd: '/w/parlar', surface: 'terminal', isInteractive: true })
  await clock.advance(2_000)
  expect(d.toasts).toEqual([])
  d.now.focused = false
  await clock.advance(2_000)
  expect(d.toasts).toEqual(['Voice moved to ginza'])
  d.now.down = true
  d.release()
  await clock.advance(100)
  expect(d.toasts[1]).toMatch(/parlard stopped/)
  // the reconnect timer finds it again
  d.now.down = false
  await clock.advance(5_000)
  expect(d.toasts[2]).toBe('parlard is back')
  d.release()
})

test('a daemon notice shows as a toast', async ($, on) => {
  const clock = mock.clock(on, { now: 1_000 })
  const d = daemon(
    on,
    [
      { ui: 'phase', phase: 'speaking', mic_muted: false, voice_off: false },
      { ui: 'notice', text: 'The mic clips on parlar own voice' },
    ],
    true,
  )
  await $.session.start({ cwd: '/w/parlar', surface: 'terminal', isInteractive: true })
  await clock.advance(100)
  expect(d.toasts).toContain('The mic clips on parlar own voice')
  d.release()
})

test('the strip budget keeps mute and stop and drops the rest to fit', () => {
  const wide = fits(166, { focused: false, voiceOff: true }, true)
  expect(wide).toEqual({ meter: true, voice: true, notFocused: true, captionsOnly: true, pane: true })
  // 80 columns, off focus, captions only, listening: the case that overflowed
  const narrow = fits(76, { focused: false, voiceOff: true }, true)
  const used =
    18 + 1 + 10 + 8 + 13 + (narrow.meter ? 9 : 0) + (narrow.voice ? 13 : 0) + (narrow.notFocused ? 17 : 0) +
    (narrow.captionsOnly ? 14 : 0) + (narrow.pane ? 8 : 0)
  expect(used).toBeLessThanOrEqual(76)
  // revuto's case: 74 columns (70 for the row), off focus, mic muted, voice on, ready
  const edge = fits(70, { focused: false, voiceOff: false }, false)
  const edgeUsed = 18 + 1 + 13 + 10 + 8 + (edge.voice ? 13 : 0) + (edge.notFocused ? 17 : 0) + (edge.pane ? 8 : 0)
  expect(edgeUsed).toBeLessThanOrEqual(70)
  expect(fits(30, { focused: true, voiceOff: false }, true)).toEqual({
    meter: false,
    voice: false,
    notFocused: false,
    captionsOnly: false,
    pane: false,
  })
})

test("parlar's note during a long call draws dim on that call's row", async ($, on) => {
  const clock = mock.clock(on, { now: 1_000 })
  const note = "Got it. I'm still on this step: run the tests. I'll pass that on when it ends."
  const d = daemon(
    on,
    [
      { ui: 'phase', phase: 'working', mic_muted: false, voice_off: false },
      { ui: 'caption', who: 'parlar', text: note, session: SESSION, call: 't9' },
    ],
    true,
  )
  await $.session.start({ cwd: '/w/parlar', surface: 'terminal', isInteractive: true })
  await clock.advance(200)
  const ui = await $.ui.mount({
    plugin: 'parlar',
    surface: 'terminal',
    component: 'ToolGroup',
    props: {
      calls: [{ tool_use_id: 't9', tool: 'Bash', input: { command: 'cargo test' }, isRunning: true, isErrored: false, isInterrupted: false }],
      isActive: true,
      isExpanded: false,
    },
  } as never)
  const line = await ui.find({ type: 'Text', text: /· Got it\. I'm still on this step/ })
  expect(line).toBeDefined()
  expect(await ui.find({ type: 'Text', text: /parlar ▸ Got it/ })).toBeUndefined()
  await ui.unmount()
  d.release()
})

const SPINNER = { component: 'Spinner', props: { word: 'Working', message: null, suffix: '…', mode: 'tool-use' } } as const
const HINT = { component: 'PromptHint', props: { isDraft: false, isWorking: false, hint: '? for shortcuts' } } as const

test('while nobody talks the strip leaves its line and rides the spinner and the hint', async ($, on) => {
  const clock = mock.clock(on, { now: 1_000 })
  let tail: string | undefined
  on('ui.render', { component: 'PromptHint' }, ($, e) => {
    tail = (e.props as { tail?: string }).tail
    return $.ui.resolve(e).Text({ children: 'hint' })
  })
  const d = daemon(on, [{ ui: 'phase', phase: 'working', mic_muted: false, voice_off: false }], true)
  await $.session.start({ cwd: '/w/parlar', surface: 'terminal', isInteractive: true })
  await clock.advance(2_000)
  const band = await $.ui.mount({ plugin: 'parlar', surface: 'terminal', ...BAND })
  expect(await band.find({ key: 'mute' })).toBeUndefined()
  await band.unmount()
  const spin = await $.ui.mount({ plugin: 'parlar', surface: 'terminal', ...SPINNER } as never)
  expect(await spin.find({ type: 'Text', text: /● voice/ })).toBeDefined()
  await spin.press({ key: 'spin-mute' })
  expect(d.ran.map(a => a.slice(1))).toContainEqual(['ctl', 'mute'])
  await spin.unmount()
  const hint = await $.ui.mount({ plugin: 'parlar', surface: 'terminal', ...HINT } as never)
  expect(tail).toMatch(/● voice ready · \/parlar/)
  await hint.unmount()
  d.release()
})
