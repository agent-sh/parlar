export type Phase = 'stopped' | 'connecting' | 'ready' | 'listening' | 'interrupting' | 'working' | 'speaking'

/** What the strip above the prompt draws, fed by `parlar ctl watch`. */
export type Strip = {
  /** False while parlard is not running. */
  connected: boolean
  phase: Phase
  muted: boolean
  voiceOff: boolean
  /** Whether this session has voice focus. */
  focused: boolean
  /** Recent levels, 0..1, of whoever is talking. */
  levels: number[]
  caption: { who: string; text: string } | null
  /** A failed tool call, shown red until it fades. */
  flare: boolean
}

/** One line of the conversation as the pane lists it. */
export type HistoryLine = { who: 'you' | 'parlar'; text: string }

/** A session parlard knows, as `parlar ctl state` lists it. */
export type SessionRow = { session: string; cwd: string; harness: string; focused: boolean }

/** An audio device, as `parlar ctl devices` lists it. */
export type Device = { id: string; name: string; current: boolean }

/** What the voice pane draws. */
export type Panel = {
  /** The conversation heard and spoken while this session had focus, oldest first. */
  history: HistoryLine[]
  sessions: SessionRow[]
  devices: { inputs: Device[]; outputs: Device[] } | null
}

/** Utterances by the transcript row that delivered them, for drawing that row. */
export type Rows = {
  /** By the uuid of a voice wake's user row. */
  wakes: Record<string, string[]>
  /** By the tool call whose PostToolUse context passed them on. */
  calls: Record<string, string[]>
  /** parlar's own notes (its answer to words heard during a long call), by that call. */
  notes?: Record<string, string[]>
}

declare module 'claude-code' {
  interface PluginState {
    parlar: { strip: Strip; rows: Rows; panel: Panel }
  }
}
