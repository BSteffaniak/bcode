# Interactive Tools Architecture

Bcode interactive tools are split into two layers:

* **Semantic interaction controllers** own state and behavior. They consume renderer-neutral `InteractionInput` values and expose typed snapshots.
* **Renderers** adapt a client environment to those semantic inputs and snapshots. Terminal renderers translate key/mouse/paste events; future browser renderers can translate DOM events.

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

`MyTerminalRenderer` implements `TerminalInteractionRenderer<MyInteraction>` and is only responsible for:

* calculating height from a snapshot
* rendering a snapshot
* mapping terminal events to `InteractionInput`

## Request metadata

Interactive tool requests carry both:

* `interaction_kind`: semantic controller kind, for clients that use snapshots and semantic inputs
* `surface_kind`: renderer-specific surface kind, for terminal/TUI rendering

For example, the question tool uses:

* `interaction_kind = "bcode.question"`
* `surface_kind = "bcode.question.inline"`

Non-terminal clients should key off `interaction_kind` and should not need BMUX or terminal event types.

## Future server/client lifecycle

The daemon/server can host interaction controllers using plugin interaction registries:

* open controller by `interaction_kind`
* return `snapshot_json()` for a pending interaction
* accept `InteractionInput`
* return updated snapshots or submitted/cancelled results

This keeps browser support straightforward without introducing a browser renderer now.
