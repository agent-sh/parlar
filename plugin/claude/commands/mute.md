---
description: Mute the mic without stopping the conversation (use "/parley:mute off" to unmute)
argument-hint: "[off]"
allowed-tools: Bash(parley ctl mute:*), Bash(parley ctl unmute:*)
---
!`if [ "$ARGUMENTS" = "off" ]; then parley ctl unmute >/dev/null 2>&1 && echo "mic on"; else parley ctl mute >/dev/null 2>&1 && echo "mic muted"; fi`

Confirm the mic state above in one short line.
