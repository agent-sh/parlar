---
description: Start a voice conversation with this session
allowed-tools: Bash(parley ctl:*), mcp__plugin_parley_parley__say
---
!`parley ctl unmute >/dev/null && parley ctl on >/dev/null && parley ctl focus ${CLAUDE_SESSION_ID} >/dev/null && parley status`

Voice mode is on and this session has voice focus. Greet the user out loud with the say tool in one short sentence, then end your turn and wait for them to speak. From now on follow the parley voice rules.
