// Modified by kdnk for wm on 2026-11-19. Original attribution is preserved in LICENSE.
use objc2_application_services::AXError;
use tracing::debug;

use crate::sys::axuielement::{AXUIElement, Error as AxError};

const ATTRIBUTE: &str = "AXEnhancedUserInterface";

trait EnhancedUiAttribute {
    fn read(&self) -> Result<bool, AxError>;
    fn write(&self, enabled: bool) -> Result<(), AxError>;
}

impl EnhancedUiAttribute for AXUIElement {
    fn read(&self) -> Result<bool, AxError> { self.bool_attribute(ATTRIBUTE) }

    fn write(&self, enabled: bool) -> Result<(), AxError> {
        self.set_bool_attribute(ATTRIBUTE, enabled)
    }
}

#[derive(Debug, Default)]
pub struct EnhancedUi {
    depth: usize,
    suppression: Suppression,
}

#[derive(Debug, Default, PartialEq, Eq)]
enum Suppression {
    #[default]
    Inactive,
    Unsupported,
    Disabled,
    RestorePending,
}

impl EnhancedUi {
    pub(crate) fn invalidate_support_cache(&mut self) {
        if self.suppression == Suppression::Unsupported {
            self.suppression = Suppression::Inactive;
        }
    }

    pub fn acquire(&mut self, app: &AXUIElement) { self.acquire_with(app); }

    fn acquire_with(&mut self, app: &impl EnhancedUiAttribute) {
        self.depth += 1;
        if matches!(
            self.suppression,
            Suppression::Disabled | Suppression::Unsupported
        ) {
            return;
        }

        match app.read() {
            Ok(true) => match set_enhanced_ui(app, false) {
                Ok(()) => self.suppression = Suppression::Disabled,
                Err(AxError::Ax(AXError::AttributeUnsupported)) => self.cache_unsupported(),
                Err(err) => {
                    // AX can apply a setter even when it returns an error.
                    self.suppression = Suppression::RestorePending;
                    debug!(?err, "Could not confirm Enhanced UI was disabled");
                }
            },
            Ok(false) if self.suppression == Suppression::RestorePending => {
                self.suppression = Suppression::Disabled;
            }
            Ok(false) | Err(AxError::NotFound) => {}
            Err(AxError::Ax(AXError::AttributeUnsupported)) => self.cache_unsupported(),
            Err(err) => debug!(?err, "Could not read Enhanced UI"),
        }
    }

    pub(crate) fn retain(&mut self) {
        debug_assert!(self.depth > 0);
        self.depth += 1;
    }

    pub fn release(&mut self, app: &AXUIElement) { self.release_with(app); }

    fn release_with(&mut self, app: &impl EnhancedUiAttribute) {
        if self.depth == 0 {
            return;
        }

        self.depth -= 1;
        if self.depth == 0 {
            self.try_restore(app);
        }
    }

    pub fn restore_if_needed(&mut self, app: &AXUIElement) {
        self.depth = 0;
        self.try_restore(app);
    }

    fn try_restore(&mut self, app: &impl EnhancedUiAttribute) {
        if !matches!(
            self.suppression,
            Suppression::Disabled | Suppression::RestorePending
        ) {
            return;
        }

        match set_enhanced_ui(app, true) {
            Ok(()) => self.suppression = Suppression::Inactive,
            Err(AxError::Ax(AXError::AttributeUnsupported)) => {
                self.suppression = Suppression::Unsupported;
            }
            Err(err) => {
                self.suppression = Suppression::RestorePending;
                debug!(?err, "Could not confirm Enhanced UI was restored");
            }
        }
    }

    fn cache_unsupported(&mut self) {
        if self.suppression == Suppression::Inactive {
            self.suppression = Suppression::Unsupported;
        }
    }
}

fn set_enhanced_ui(app: &impl EnhancedUiAttribute, enabled: bool) -> Result<(), AxError> {
    match app.write(enabled) {
        // AppKit can apply the change and still return NotImplemented.
        Err(AxError::Ax(AXError::NotImplemented)) if app.read().ok() == Some(enabled) => Ok(()),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    struct Attribute {
        enabled: Cell<bool>,
        read_error: Cell<Option<AxError>>,
        write_error: Cell<Option<AXError>>,
    }

    impl Attribute {
        fn enabled() -> Self {
            Self {
                enabled: Cell::new(true),
                read_error: Cell::default(),
                write_error: Cell::default(),
            }
        }
    }

    impl EnhancedUiAttribute for Attribute {
        fn read(&self) -> Result<bool, AxError> {
            self.read_error.take().map_or_else(|| Ok(self.enabled.get()), Err)
        }

        fn write(&self, enabled: bool) -> Result<(), AxError> {
            self.enabled.set(enabled);
            self.write_error.take().map_or(Ok(()), |error| Err(AxError::Ax(error)))
        }
    }

    #[test]
    fn next_batch_retries_suppression_during_an_animation() {
        let app = Attribute::enabled();
        let mut eui = EnhancedUi::default();
        app.read_error.set(Some(AxError::Ax(AXError::CannotComplete)));
        eui.acquire_with(&app);
        eui.retain();
        eui.release_with(&app);
        assert!(app.enabled.get());

        eui.acquire_with(&app);
        assert!(!app.enabled.get());
        eui.release_with(&app);
        assert!(!app.enabled.get());
        eui.release_with(&app);
        assert!(app.enabled.get());
    }

    #[test]
    fn temporarily_missing_value_is_rechecked() {
        let app = Attribute::enabled();
        let mut eui = EnhancedUi::default();
        app.read_error.set(Some(AxError::NotFound));
        eui.acquire_with(&app);
        eui.release_with(&app);

        eui.acquire_with(&app);
        assert!(!app.enabled.get());
        eui.release_with(&app);
        assert!(app.enabled.get());
    }

    #[test]
    fn restores_after_an_ax_error_that_applied_the_write() {
        let app = Attribute::enabled();
        let mut eui = EnhancedUi::default();
        app.write_error.set(Some(AXError::CannotComplete));
        eui.acquire_with(&app);
        assert!(!app.enabled.get());
        eui.release_with(&app);
        assert!(app.enabled.get());
    }

    #[test]
    fn new_window_rechecks_an_unsupported_attribute() {
        let app = Attribute::enabled();
        let mut eui = EnhancedUi::default();
        app.read_error.set(Some(AxError::Ax(AXError::AttributeUnsupported)));
        eui.acquire_with(&app);
        eui.release_with(&app);

        eui.acquire_with(&app);
        assert!(app.enabled.get());
        eui.release_with(&app);

        eui.invalidate_support_cache();
        eui.acquire_with(&app);
        assert!(!app.enabled.get());
        eui.release_with(&app);
        assert!(app.enabled.get());
    }

    #[test]
    fn next_batch_suppresses_after_an_unconfirmed_restore() {
        let app = Attribute::enabled();
        let mut eui = EnhancedUi::default();
        eui.acquire_with(&app);
        app.write_error.set(Some(AXError::CannotComplete));
        eui.release_with(&app);
        assert!(app.enabled.get());

        eui.acquire_with(&app);
        assert!(!app.enabled.get());
        eui.release_with(&app);
        assert!(app.enabled.get());
    }
}
