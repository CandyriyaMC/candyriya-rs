//! Internal-only command. It is not part of the brigadier tree, not part of
//! any usage listing, and not suggested by tab completion; the permission and
//! i18n machinery does not apply to it. Handled by the pre-dispatch hook in
//! `Dispatcher::execute`.

use crate::dispatcher::HiddenRunner;
use crate::source::CommandSource;

use crate::effects::ProxyEffects;

/// Handles the one internal command the proxy carries. Any input whose
/// first word does not match is declined, so it passes through to the normal
/// command tree untouched.
pub(crate) struct StaticHidden {
    /// The effects sink — the line goes out through the one output path.
    pub(crate) effects: std::sync::Arc<dyn ProxyEffects>,
}

impl HiddenRunner for StaticHidden {
    fn run(&self, input: &str, source: &CommandSource) -> Option<i32> {
        match input.split_whitespace().next() {
            Some("arina") => {
                self.effects.send(source, "she left 01.01.26 :(");
                Some(1)
            }
            _ => None,
        }
    }
}
