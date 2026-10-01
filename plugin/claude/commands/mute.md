---
description: Mute or unmute the mic without stopping the conversation
argument-hint: "[on|off]"
allowed-tools: Bash(parley ctl:*)
---
!`if [ "$ARGUMENTS" = "off" ]; then parley ctl unmute >/dev/null; else parley ctl mute >/dev/null; fi; parley status`

Confirm the mic state above in one short line.
