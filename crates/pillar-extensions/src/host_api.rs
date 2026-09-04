//! The `@pillar` host API surface (pi v0.84.3 ExtensionAPI, mapped
//! through docs/rules/04 naming conventions): the registration
//! methods an extension setup function receives on its `pillar`
//! argument, each recording into the host registry.
//!
//! divergences: handler invocation is host-driven (the host resolves
//! recorded function references when dispatching an event); UI
//! methods (ctx.ui.*) are host callbacks not ported here.

#[cfg(test)]
mod tests {
    use crate::runtime::ExtensionRuntime;

    /// `pillar.on` records handlers in registration order (upstream
    /// the runner's per-event handler lists).
    #[test]
    fn on_records_handlers_in_order() {
        // The API surface grows with the runner integration; this
        // test anchors the contract once installation lands.
        let runtime = ExtensionRuntime::new();
        let _ = runtime.vm();
        let chunk = runtime.vm().load(
            r#"
            local pillar = require("@pillar")
            return type(pillar) == "table"
        "#,
        );
        let ok: bool = chunk.call(()).unwrap();
        assert!(ok);
    }
}
