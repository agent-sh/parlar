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

declare module 'claude-code' {
  interface PluginState {
    parlar: { strip: Strip }
  }
}
