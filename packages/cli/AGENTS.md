# Blims CLI/TUI Agent Guidelines

These instructions apply when editing Blims CLI or TUI code in this package.

## Same Domain Action, Native Frontend Manifestations

Blims CLI and TUI code must preserve the frontend-agnostic Blims architecture.

Shared Blims actions should return structured state, handles, events, or updates. They must not print, attach to another terminal UI, enter/leave terminal modes, or assume a specific frontend.

Frontend-specific adapters are responsible for presentation:

* CLI adapters may print stdout, emit JSON, and optionally attach/follow a Bcode session.
* TUI adapters must render in-game panels, modals, overlays, input boxes, and game interactions without surrendering terminal ownership to another UI.

## Conversation Rule

Do not call `attach_session` from inside the Blims TUI loop or from a frontend-neutral helper.

Agent conversation code should be split into:

* a shared conversation start action that creates/records the conversation and returns a conversation handle
* a CLI manifestation that prints/attaches when invoked by `bcode blims talk`
* a TUI manifestation that opens an in-game conversation surface and polls/renders Bcode session history natively

If an interaction starts in `bcode blims enter`, it must remain inside the Blims TUI unless the user explicitly chooses to leave the game.
