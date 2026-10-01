---
description: Mute the mic without stopping the conversation (use "/parlar:mute off" to unmute)
argument-hint: "[off]"
allowed-tools: Bash(parlar ctl mute:*), Bash(parlar ctl unmute:*)
---
!`if [ "$ARGUMENTS" = "off" ]; then parlar ctl unmute >/dev/null 2>&1 && echo "mic on"; else parlar ctl mute >/dev/null 2>&1 && echo "mic muted"; fi`

Confirm the mic state above in one short line.
