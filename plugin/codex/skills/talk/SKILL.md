---
name: talk
description: "Start a voice conversation with this Codex session through parlar. Use when the user asks to talk, start voice mode, or switch voice to this session."
---

Run this shell command (it needs the parlar socket under `$XDG_RUNTIME_DIR`, so ask for approval if
the sandbox blocks it):

```
parlar ctl talk --session "$CODEX_THREAD_ID" --harness codex 2>&1 || echo PARLAR_SETUP_NEEDED
```

If the output contains PARLAR_SETUP_NEEDED, parlar is not installed or its daemon is not running.
Do not call the say tool. Tell the user in two or three sentences what is missing: install with
`npm install -g @agent-sh/parlar`, then `parlard fetch` and `parlard service`.

Otherwise voice mode is on and this session has voice focus. Greet the user out loud with the
parlar say tool in one short sentence, then end your turn and wait for them to speak. If say
answers "Not spoken", tell the user in one line of text that voice focus is elsewhere and stop;
do not try other ways to reach parlar.
