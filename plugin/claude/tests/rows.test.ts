import { expect, test } from 'claude-code/testing'
import type { On } from 'claude-code'

import { heardIn, notesOf } from '../hooks/rows'

const SAY = 'mcp__plugin_parlar_parlar__say'
const WAKE_TEXT =
  '<task-notification>\n<summary>voice</summary>\n</task-notification>\n<system-reminder>\n' +
  'The user spoke to you by voice: [voice u1] Voice mode is on. Talk to the user through the say tool.\n' +
  'list the files\n(Reply out loud with the say tool.)\n</system-reminder>'

// the module's session.append hook notes the row before the chain goes on; nothing beneath it
// stores the row in a test, so the call itself rejects
async function append($: { session: { append: (e: never) => Promise<unknown> } }, e: object) {
  await $.session.append(e as never).catch(() => undefined)
}

function engine(on: On) {
  on('ui.render', ($, e) => $.ui.resolve(e).Text({ children: 'engine' }))
}

test('heardIn takes the utterances and drops the reminder and notes', () => {
  expect(heardIn(WAKE_TEXT)).toEqual(['list the files'])
  const two =
    '[voice u4] open it\n(heard: "open it um". Speech recognition.)\n[voice u5 continues u4] the router\n(Reply out loud.)'
  expect(heardIn(two)).toEqual(['open it', 'the router'])
  expect(heardIn('Said.')).toEqual([])
})

test('notesOf maps wakes, tool results and hook context to their rows', () => {
  const wake = notesOf(
    { door: 'prompt', origin: { kind: 'task-notification' }, uuid: 'row-1', message: { content: WAKE_TEXT } },
    '',
  )
  expect(wake.notes).toEqual([{ kind: 'wakes', key: 'row-1', heard: ['list the files'] }])
  const result = notesOf(
    {
      door: 'tool-result',
      origin: { kind: 'tool' },
      uuid: 'r-1',
      message: { content: [{ type: 'tool_result', tool_use_id: 'b1', content: 'ok' }] },
    },
    '',
  )
  expect(result).toEqual({ notes: [], lastCall: 'b1' })
  const context = notesOf(
    {
      door: 'hook-context',
      origin: { kind: 'hook' },
      uuid: 'r-2',
      message: {
        name: 'hook_additional_context',
        content: [{ type: 'text', text: 'PostToolUse:Bash hook additional context: [voice u3] stop\n(Reply out loud.)' }],
      },
    },
    result.lastCall,
  )
  expect(context.notes).toEqual([{ kind: 'calls', key: 'b1', heard: ['stop'] }])
})

test('a voice wake row draws what was heard', async ($, on) => {
  engine(on)
  await append($, { door: 'prompt', origin: { kind: 'task-notification' }, uuid: 'row-1', message: { type: 'user', role: 'user', content: WAKE_TEXT } })
  const ui = await $.ui.mount({
    plugin: 'parlar',
    surface: 'terminal',
    component: 'UserMessage',
    requestId: 'row-1',
    props: { text: 'voice', origin: { kind: 'task-notification' }, isExpanded: false },
  } as never)
  expect(await ui.find({ type: 'Text', text: /you ▸ list the files/ })).toBeDefined()
  await ui.unmount()
})

test('say calls draw as parlar lines, with what they passed on', async ($, on) => {
  engine(on)
  await append($, {
    door: 'tool-result',
    origin: { kind: 'tool', tool: SAY },
    uuid: 'r-1',
    message: {
      type: 'user',
      role: 'user',
      content: [{ type: 'tool_result', tool_use_id: 't1', content: [{ type: 'text', text: 'Said.\nThe user said meanwhile:\n[voice u2] and the docs\n(Reply out loud.)' }] }],
    },
  })
  const calls = [
    { tool_use_id: 't1', tool: SAY, input: { text: 'Opening it.' }, isRunning: false, isErrored: false, isInterrupted: false },
    {
      tool_use_id: 't2',
      tool: SAY,
      input: { text: 'Nobody hears this.' },
      output: [{ type: 'text', text: 'Not spoken: no focus' }],
      isRunning: false,
      isErrored: false,
      isInterrupted: false,
    },
  ]
  for (const surface of ['terminal', 'desktop'] as const) {
    const ui = await $.ui.mount({
      plugin: 'parlar',
      surface,
      component: 'ToolGroup',
      props: { calls, isActive: false, isExpanded: false },
    } as never)
    expect(await ui.find({ type: 'Text', text: /parlar ▸ Opening it\./ })).toBeDefined()
    expect(await ui.find({ type: 'Text', text: /you ▸ and the docs/ })).toBeDefined()
    expect(await ui.find({ type: 'Text', text: /\(not spoken\)/ })).toBeDefined()
    expect(await ui.find({ type: 'Text', text: /engine/ })).toBeUndefined()
    await ui.unmount()
  }
})

test('a group with other calls keeps the engine summary above the lines', async ($, on) => {
  engine(on)
  await append($, {
    door: 'tool-result',
    origin: { kind: 'tool', tool: 'Bash' },
    uuid: 'r-2',
    message: { type: 'user', role: 'user', content: [{ type: 'tool_result', tool_use_id: 'b1', content: 'ok' }] },
  })
  await append($, {
    door: 'hook-context',
    origin: { kind: 'hook', event: 'PostToolUse' },
    uuid: 'r-3',
    message: {
      type: 'attachment',
      name: 'hook_additional_context',
      role: 'user',
      content: [{ type: 'text', text: 'PostToolUse:Bash hook additional context: [voice u3] stop after this\n(Reply out loud.)' }],
    },
  })
  const ui = await $.ui.mount({
    plugin: 'parlar',
    surface: 'terminal',
    component: 'ToolGroup',
    props: {
      calls: [{ tool_use_id: 'b1', tool: 'Bash', input: { command: 'ls' }, isRunning: false, isErrored: false, isInterrupted: false }],
      isActive: false,
      isExpanded: false,
    },
  } as never)
  expect(await ui.find({ type: 'Text', text: /engine/ })).toBeDefined()
  expect(await ui.find({ type: 'Text', text: /you ▸ stop after this/ })).toBeDefined()
  await ui.unmount()
})
