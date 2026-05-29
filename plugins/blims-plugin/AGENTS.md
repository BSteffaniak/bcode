# Blims Plugin Agent Guidelines

These instructions apply to Blims plugin/domain work.

## Frontend-Agnostic Product Architecture

Blims domain behavior must be presentation-neutral.

A Blims capability such as conversation, initiative creation, permission review, artifact review, agent status inspection, or work proposal handling must be modeled as a domain/application action that returns structured state, events, handles, or updates.

Do not make a domain action assume a specific UI surface such as:

* stdout
* terminal alternate screen
* TUI rendering
* Bcode session attach
* web DOM
* 3D scene objects

Each frontend must manifest the same domain action natively:

* CLI: stdout, JSON, shell-friendly prompts, optional attach/follow
* TUI: in-game panels, modals, overlays, input boxes, game interactions
* Web: routes/components/events
* 3D: world objects, diegetic interactions, spatial UI

Domain actions return structured state/events. Frontends manifest those events natively. No domain action may assume stdout, alternate screen, web DOM, or 3D scene ownership.

## Conversation Rule

Starting a Blims agent conversation must be split into:

* frontend-neutral conversation creation/session orchestration
* frontend-specific presentation

The shared action may:

* request an agent talk prompt
* create or reuse a Bcode session
* send the initial message
* record the Blims conversation
* return a structured conversation handle

The shared action must not:

* print to stdout
* call `attach_session`
* enter or leave terminal modes
* render UI
* assume synchronous terminal ownership

CLI talk may attach or print because it is the CLI manifestation. TUI talk must stay inside the Blims TUI and render an in-game conversation surface.
