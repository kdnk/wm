// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
//! Source documents are editable; runtime configs contain expanded hotkeys.
mod document;
mod parse;
mod types;
pub use document::ConfigDocument;
pub use types::*;
#[cfg(test)]
mod tests;
