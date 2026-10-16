//! Window-scoped capture used by remote-desktop clients.
use super::*;
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1,
    zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1,
};
use wayland_protocols::wp::pointer_constraints::zv1::client::{
    zwp_confined_pointer_v1::ZwpConfinedPointerV1, zwp_pointer_constraints_v1::Lifetime,
};

pub(super) struct Capture {
    pub object: TaskObjectId,
    pub confined: ZwpConfinedPointerV1,
    pub shortcuts: Option<ZwpKeyboardShortcutsInhibitorV1>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.confined.destroy();
        if let Some(inhibitor) = &self.shortcuts {
            inhibitor.destroy();
        }
    }
}

pub(super) fn shortcut(code: u32, modifiers: Modifiers) -> bool {
    code == 50 && modifiers.ctrl && modifiers.alt && !modifiers.logo
}

impl App {
    pub(super) fn toggle_capture(&mut self, qh: &QueueHandle<Self>) {
        if self.immersion.is_some() {
            self.end_capture();
            return;
        }
        let Some(object) = self.focused else {
            return;
        };
        let Some(surface) = self
            .tasks
            .get(&object)
            .and_then(TaskWindow::surface)
            .cloned()
        else {
            return;
        };
        let Some(pointer) = self.pointer.as_ref() else {
            return;
        };
        let confined = match self.pointer_constraints.confine_pointer(
            &surface,
            pointer.pointer(),
            None,
            Lifetime::Persistent,
            qh,
        ) {
            Ok(confined) => confined,
            Err(error) => {
                eprintln!("Droidloom capture unavailable: {error}");
                return;
            }
        };
        let shortcuts = self
            .shortcuts_inhibit_manager
            .as_ref()
            .zip(self.keyboard_seat.as_ref())
            .map(|(manager, seat)| manager.inhibit_shortcuts(&surface, seat, qh, ()));
        eprintln!(
            "Droidloom capture requested object={} shortcuts-inhibited={}",
            object.0,
            shortcuts.is_some()
        );
        self.immersion = Some(Capture {
            object,
            confined,
            shortcuts,
        });
        self.capture_title(object, true);
    }

    pub(super) fn end_capture(&mut self) {
        if let Some(capture) = self.immersion.take() {
            let object = capture.object;
            drop(capture);
            self.capture_title(object, false);
            eprintln!("Droidloom capture released object={}", object.0);
        }
    }

    fn capture_title(&self, object: TaskObjectId, captured: bool) {
        if let Some(task) = self.tasks.get(&object) {
            if let Some(window) = &task.window {
                let title = if captured {
                    format!("{} [Mouse captured: Ctrl+Alt+M to release]", task.package)
                } else {
                    task.package.clone()
                };
                window.set_title(title);
            }
        }
    }
}

impl smithay_client_toolkit::seat::pointer_constraints::PointerConstraintsHandler for App {
    fn confined(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        confined: &ZwpConfinedPointerV1,
        _: &wl_surface::WlSurface,
        _: &wl_pointer::WlPointer,
    ) {
        if self
            .immersion
            .as_ref()
            .is_some_and(|capture| capture.confined == *confined)
        {
            eprintln!("Droidloom capture active");
        }
    }
    fn unconfined(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ZwpConfinedPointerV1,
        _: &wl_surface::WlSurface,
        _: &wl_pointer::WlPointer,
    ) {
    }
    fn locked(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wayland_protocols::wp::pointer_constraints::zv1::client::zwp_locked_pointer_v1::ZwpLockedPointerV1,
        _: &wl_surface::WlSurface,
        _: &wl_pointer::WlPointer,
    ) {
    }
    fn unlocked(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wayland_protocols::wp::pointer_constraints::zv1::client::zwp_locked_pointer_v1::ZwpLockedPointerV1,
        _: &wl_surface::WlSurface,
        _: &wl_pointer::WlPointer,
    ) {
    }
}

wayland_client::delegate_noop!(App: ignore ZwpKeyboardShortcutsInhibitManagerV1);
wayland_client::delegate_noop!(App: ignore ZwpKeyboardShortcutsInhibitorV1);
smithay_client_toolkit::delegate_pointer_constraints!(App);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capture_has_an_explicit_keyboard_escape_chord() {
        let keys = Modifiers {
            ctrl: true,
            alt: true,
            ..Modifiers::default()
        };
        assert!(shortcut(50, keys));
        assert!(!shortcut(50, Modifiers { logo: true, ..keys }));
        assert!(!shortcut(
            50,
            Modifiers {
                ctrl: false,
                ..keys
            }
        ));
        assert!(!shortcut(50, Modifiers { alt: false, ..keys }));
        assert!(!shortcut(30, keys));
    }
}
