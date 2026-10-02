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
  on('process.spawn', async function* () {
    for (const ev of events) yield { stream: 'stdout' as const, text: JSON.stringify(ev) + '\n' }
    await held
    return { value: { code: 0, signal: null } }
  })
  on('process.run', ($, e) => {
    ran.push([...e.argv])
    const state = { sessions: [{ session: SESSION, focused }] }
    const stdout = e.argv[2] === 'state' ? JSON.stringify(state) : '{"kind":"ok"}'
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
