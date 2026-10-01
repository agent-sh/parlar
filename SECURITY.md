# Security Policy

## Reporting vulnerabilities

Do not open public issues for security vulnerabilities. Use GitHub private vulnerability reporting
on this repository, or contact the maintainer directly through GitHub.

## Supported versions

Only the latest release gets security fixes.

## Scope

parlar listens to the microphone and passes what it hears to a coding-agent session, which can run
tools. In scope:

- audio or transcripts leaving the machine (parlar sends neither anywhere; the models run locally)
- speech reaching a session that does not have voice focus
- the control socket (`$XDG_RUNTIME_DIR/parlar/parlar.sock`) being reachable by another user
- model and library downloads that skip their pinned sha256 checks
- hook behavior that could block or alter a harness session when parlard is not running
