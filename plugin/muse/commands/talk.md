---
description: Start a voice conversation with this session
allowed-tools: Bash(parlar ctl talk:*), mcp__plugin_parlar_parlar__say
---
!`parlar ctl talk --session "${MUSE_SESSION_ID:-$CLAUDE_SESSION_ID}" --harness other 2>&1 || echo PARLAR_SETUP_NEEDED`

If the output above contains PARLAR_SETUP_NEEDED, parlar is not installed or its daemon is not running. Do not call the say tool. In two or three sentences tell the user what is missing and offer to set it up for them with /parlar:setup.

Otherwise voice mode is on and this session has voice focus. Greet the user out loud with the say tool in one short sentence, then end your turn and wait for them to speak.

In Muse, spoken messages arrive as context updates inside this same turn, not as separate user messages. Treat the newest spoken message as the user's latest message, superseding earlier spoken messages, and answer it directly. Never write a voice transcript line yourself.
