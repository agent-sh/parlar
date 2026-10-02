import { expect, mock, test } from 'claude-code/testing'
import type { On } from 'claude-code'

const SESSION = 'abc-123'
const BAND = { component: 'AbovePrompt', props: { hasSurvey: false, isWorking: false, maxRows: 10, bodyColumns: 120, scroll: { offset: 0, bodyRows: 9 }, view: {} } } as const

// a parlard that sends `events` on the watch stream, then holds it open
function daemon(on: On, events: object[], focused: boolean) {
  const ran: string[][] = []
  let release = () => {}
  const held = new Promise<void>(r => (release = r))
  on('session.start', ($, e) => ({ cwd: e.cwd }))
  on('ui.render', ($, e) => $.ui.resolve(e).Box({}))
  on('session.id', async () => ({ value: SESSION }))
  on('env.set', async () => ({ value: undefined }))
  on('command.register', async () => ({ value: { command: 'parlar' } }))
  on('ui.open', async () => ({ value: { isPlaced: true } }))
  on('process.spawn', async function* () {
    for (const ev of events) yield { stream: 'stdout' as const, text: JSON.stringify(ev) + '\n' }
    await held
    return { value: { code: 0, signal: null } }
  })
  on('process.run', ($, e) => {
    ran.push([...e.argv])
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
  return { ran, release }
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
  const d = daemon(on, [{ ui: 'phase', phase: 'ready', mic_muted: false, voice_off: false }], false)
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
        // the first caption lands before the focus poll: not this session's yet
        { ui: 'caption', who: 'user', text: 'too early' },
      ],
      true,
    )
    await $.session.start({ cwd: '/w/parlar', surface, isInteractive: true })
    await clock.advance(2_000)
    const r = await $.command.run({ command: 'parlar' } as never)
    expect(String((r as { text?: string }).text)).toMatch(/opened/)
    const ui = await $.ui.mount({ plugin: 'parlar', surface, ...PANE } as never)
    expect(await ui.find({ type: 'Text', text: /parlar \(this one\)/ })).toBeDefined()
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
