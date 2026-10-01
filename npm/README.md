# @agent-sh/parlar

npm package for [parlar](https://github.com/agent-sh/parlar): a voice conversation mode for Claude
Code and Codex. Install it, and parlar and parlard come from the matching GitHub release, checked
by sha256.

```
npm install -g @agent-sh/parlar
parlard fetch      # speech models and libmoonshine, about 800 MB, once
parlard service    # run parlard as a systemd user service
```

Then, in Claude Code:

```
/plugin marketplace add agent-sh/parlar
/plugin install parlar@parlar
```

If your npm blocks install scripts, add `--allow-scripts=@agent-sh/parlar`; without the script
the package has no binaries.

Linux only, x64 and arm64. The full documentation is in the
[repository README](https://github.com/agent-sh/parlar#readme).

Environment variables for the installer:

| Variable | Effect |
|---|---|
| `PARLAR_NPM_SKIP_DOWNLOAD=1` | install no binaries |
| `PARLAR_NPM_LOCAL_DIR=<dir>` | copy `parlar` and `parlard` from a local build instead |
| `PARLAR_NPM_DOWNLOAD_BASE=<url>` | download the release assets from a mirror |
