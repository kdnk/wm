// Modified by kdnk for wm on 2026-11-19. Original attribution is preserved in LICENSE.
use objc2_application_services::AXError;

use crate::sys::axuielement::{AXUIElement, Error as AxError};

const ATTRIBUTE: &str = "AXEnhancedUserInterface";

#[derive(Debug, Default)]
pub struct EnhancedUi {
    depth: usize,
    restore: bool,
    absent: bool,
}

impl EnhancedUi {
    pub fn acquire(&mut self, app: &AXUIElement) {
        if self.depth > 0 {
            self.depth += 1;
            return;
        }

        self.depth = 1;

        if self.restore || self.absent {
            return;
        }

        match app.bool_attribute(ATTRIBUTE) {
            Ok(true) => match set_enhanced_ui(app, false) {
                Ok(()) => self.restore = true,
                Err(err) if is_absent(&err) => self.absent = true,
                Err(_) => {}
            },
            Ok(false) => {}
            Err(err) if is_absent(&err) => self.absent = true,
            Err(_) => {}
        }
    }

    pub fn release(&mut self, app: &AXUIElement) {
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

    fn try_restore(&mut self, app: &AXUIElement) {
        if !self.restore {
            return;
        }

        match set_enhanced_ui(app, true) {
            Ok(()) => self.restore = false,
            Err(err) if is_absent(&err) => {
                self.restore = false;
                self.absent = true;
            }
            Err(_) => {}
        }
    }
}

fn set_enhanced_ui(app: &AXUIElement, enabled: bool) -> Result<(), AxError> {
    match app.set_bool_attribute(ATTRIBUTE, enabled) {
        // AppKit can apply the change and still return NotImplemented.
        Err(AxError::Ax(AXError::NotImplemented))
            if app.bool_attribute(ATTRIBUTE).ok() == Some(enabled) =>
        {
            Ok(())
        }
        result => result,
    }
}

#[inline]
fn is_absent(err: &AxError) -> bool {
    match err {
        AxError::NotFound => true,
        AxError::Ax(code) => *code == AXError::AttributeUnsupported,
    }
}
