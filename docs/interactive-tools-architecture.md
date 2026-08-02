# Interactive Tools Architecture

Bcode interactive tools are split into three ownership layers:

* **Semantic interaction controllers** own state and behavior. They consume renderer-neutral
  `InteractionInput` values and expose typed snapshots.
* **Shared session view** projects exchange request and terminal resolution events into one stable
  renderer-neutral `Interaction` transcript item.
* **Renderers** adapt a client environment to those semantic inputs and snapshots. Terminal
  renderers translate key, mouse, and paste events; future browser renderers can translate DOM
  events.

This intentionally avoids a generic component protocol. Plugins model their own domain snapshots.

## Plugin author path

Most plugins should implement `bcode_plugin_sdk::interaction::PluginInteraction`:

```rust,ignore
impl PluginInteraction for MyInteraction {
    const KIND: &'static str = "my.plugin.interaction";

    type Request = MyRequest;
    type Snapshot = MySnapshot;

    fn new(request: Self::Request) -> Self { /* ... */ }
    fn snapshot(&self) -> Self::Snapshot { /* ... */ }
    fn handle_input(&mut self, input: InteractionInput) -> InteractionOutput { /* ... */ }
}
```

Registration is one line:

```rust,ignore
registry.register_interaction::<MyInteraction>();
```

Terminal rendering is optional and separate:

```rust,ignore
registry.register_interactive_surface::<MyInteraction, MyTerminalRenderer>();
```

`MyTerminalRenderer` implements `TerminalInteractionRenderer<MyInteraction>` and is only responsible
for:

* calculating bounded height from a snapshot
* rendering a snapshot
* mapping terminal events to `InteractionInput`

Terminal plugin surfaces may ask `PluginTuiHost` to resolve a key stroke into the host's configured
composer-like edit command, selection motion, or submit intent. This keeps configured editor
bindings consistent without exposing Bcode keymap enums to plugin controllers or shared session
contracts.

## Request metadata

Interactive tool requests carry a producer ID plus a versioned exchange schema and opaque payload.
A selected frontend adapter contributes an `interaction_kind`, the semantic controller kind used by
clients that support native interaction snapshots and inputs.

Native surface identity is not stored in the shared session-view contract. Each frontend resolves
its own adapter from the producer ID, exchange schema/version, and platform ID. A TUI adapter may
then provide a native `tui_surface_kind`; web clients may select a different controller or native
presentation from the same shared exchange envelope.

For example, the question adapter resolves:

* `interaction_kind = "bcode.question"`
* `tui_surface_kind = "bcode.question.inline"`

The durable exchange event remains canonical. `SessionView` first projects producer ID, exchange
schema/version, response policy, and opaque payload without interpreting plugin behavior. A client
adapter may then enrich the same interaction item with its semantic interaction kind while resolving
native presentation only at its frontend boundary. Resolution updates that same transcript identity
rather than creating renderer-local history.

Non-terminal clients should key off `interaction_kind` and should not need BMUX or terminal event
types.

## TUI presentation

The TUI owns interaction placement, clipping, hit testing, mouse-coordinate translation, cursor
adaptation, and terminal painting. The same plugin controller and surface can be presented:

* inline at the semantic interaction transcript item, using the indexed transcript as the only
  scrolling authority; or
* pinned above the composer as an overlay that does not alter transcript layout.

Inline surfaces reserve bounded transcript rows. Partially visible surfaces render through a bounded
scratch frame and are clipped into the viewport; plugin-local coordinates remain stable. Pinned
surfaces are underpainted by the host before plugin rendering. Neither mode owns canonical answers
or exchange lifecycle.

## Lifecycle and failure behavior

* Requests and terminal resolutions share one stable interaction transcript ID.
* Controller validation and answer construction remain plugin-owned.
* Failed response delivery retains controller and renderer-local text-edit state for retry.
* Remote resolution closes the active surface and updates the semantic transcript item.
* Unknown adapters remain generic, bounded transcript items rather than being guessed as a known
  interaction type.

## Future server/client lifecycle

The daemon/server can host interaction controllers using plugin interaction registries:

* open controller by `interaction_kind`
* return `snapshot_json()` for a pending interaction
* accept `InteractionInput`
* return updated snapshots or submitted/cancelled results

This keeps browser support straightforward without introducing a browser renderer now.
