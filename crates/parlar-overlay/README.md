# parlar-overlay

parlar's floating voice indicator for Windows: forty fireflies above every window. Blue is you,
amber is the agent, a red spark is a failed tool call. Click to stop or start the conversation,
drag to move, right-click for sessions, devices, mute and voice off.

It talks to parlard over parlar's named pipe and has no GUI framework: a layered, topmost Win32
window drawn with GDI, about one frame every 33 ms while anything moves, idle otherwise.

Part of [parlar](https://github.com/agent-sh/parlar). `parlard service` on Windows starts it at
login along with parlard. MIT or Apache-2.0.
