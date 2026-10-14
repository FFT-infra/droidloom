//! Navigation controls and edge gestures for task windows without a titlebar.
use super::*;

impl App {
    pub(super) fn configure_controls(
        &mut self,
        qh: &QueueHandle<Self>,
        object: TaskObjectId,
        width: u32,
        scale: u32,
    ) {
        let Some(task) = self.tasks.get_mut(&object) else {
            return;
        };
        if task.headless() {
            return;
        }
        if task.applied_scale_120 != scale {
            task.chrome = None;
            task.gesture_feedback = None;
        }
        let Some(parent) = task.surface().cloned() else {
            return;
        };
        if task.chrome.is_none() {
            match chrome::Chrome::new(
                &self.layer_globals,
                &self.compositor,
                &parent,
                qh,
                scale,
                WindowManagerCapabilities::empty(),
            ) {
                Ok(chrome) => task.chrome = Some(chrome),
                Err(error) => eprintln!("Droidloom window controls: {error}"),
            }
        }
        if let Some(chrome) = task.chrome.as_mut() {
            chrome.set_active(task.focused);
            chrome.place(
                width,
                0,
                chrome::Presentation::for_window(task.fullscreen, true),
                task.controls_until.is_some(),
            );
        }
    }

    pub(super) fn control_at(
        &self,
        surface: &wl_surface::WlSurface,
        position: (f64, f64),
    ) -> Option<(TaskObjectId, chrome::Action)> {
        self.tasks.iter().find_map(|(object, task)| {
            task.chrome
                .as_ref()?
                .action_at(&surface.id(), position.0, position.1)
                .map(|action| (*object, action))
        })
    }

    pub(super) fn clear_control_presses(&mut self) {
        self.chrome_press = None;
        self.chrome_touches.clear();
        for task in self.tasks.values_mut() {
            if let Some(chrome) = task.chrome.as_mut() {
                chrome.reset();
            }
        }
    }

    pub(super) fn control_action(&mut self, object: TaskObjectId, action: chrome::Action) {
        match action {
            chrome::Action::Back => self.back_key(object),
            chrome::Action::Fullscreen => {
                if let Some(task) = self.tasks.get(&object)
                    && let Some(window) = task.window.as_ref()
                {
                    if task.fullscreen {
                        window.unset_fullscreen();
                    } else {
                        window.set_fullscreen(None);
                    }
                }
                self.reveal_controls(object);
            }
        }
    }

    fn back_key(&mut self, object: TaskObjectId) {
        // KEY_BACK is converted to Android KEYCODE_BACK by the native receiver.
        for action in [KeyAction::Down, KeyAction::Up] {
            if let Err(error) = self.send_input(
                object,
                InputEvent::Key {
                    action,
                    keycode: 158,
                    repeat: 0,
                },
            ) {
                self.fail(&error);
            }
        }
    }

    pub(super) fn control_pointer(&mut self, event: &PointerEvent) -> bool {
        let hit = self.control_at(&event.surface, event.position);
        match event.kind {
            PointerEventKind::Press {
                button: LEFT_BUTTON,
                ..
            } if hit.is_some() => {
                self.chrome_press = hit;
                let (object, action) = hit.unwrap();
                if let Some(chrome) = self
                    .tasks
                    .get_mut(&object)
                    .and_then(|task| task.chrome.as_mut())
                {
                    chrome.press(action);
                }
                true
            }
            PointerEventKind::Release {
                button: LEFT_BUTTON,
                ..
            } if self.chrome_press.is_some() => {
                let pressed = self.chrome_press.take();
                self.clear_control_presses();
                if pressed == hit
                    && let Some((object, action)) = hit
                {
                    self.control_action(object, action);
                }
                true
            }
            PointerEventKind::Leave { .. } if hit.is_some() => {
                self.clear_control_presses();
                true
            }
            _ if hit.is_some() => {
                let (object, action) = hit.unwrap();
                if self.chrome_press.is_none()
                    && let Some(chrome) = self
                        .tasks
                        .get_mut(&object)
                        .and_then(|task| task.chrome.as_mut())
                {
                    chrome.hover(action);
                }
                true
            }
            _ => false,
        }
    }

    pub(super) fn reveal_controls(&mut self, object: TaskObjectId) {
        if let Some(task) = self.tasks.get_mut(&object) {
            task.controls_until = Some(Instant::now() + Duration::from_secs(4));
            if let Some(chrome) = task.chrome.as_mut() {
                chrome.set_controls_revealed(true);
            }
        }
    }

    pub(super) fn expire_controls(&mut self) {
        let now = Instant::now();
        for task in self.tasks.values_mut() {
            if task.controls_until.is_some_and(|until| until <= now) {
                task.controls_until = None;
                if let Some(chrome) = task.chrome.as_mut() {
                    chrome.set_controls_revealed(false);
                }
            }
        }
    }

    fn hide_feedback(&mut self, object: TaskObjectId) {
        if let Some(feedback) = self
            .tasks
            .get_mut(&object)
            .and_then(|task| task.gesture_feedback.as_mut())
        {
            feedback.hide();
        }
    }

    pub(super) fn abandon_gestures(&mut self) {
        if let Some(candidate) = self.swipe_back.take() {
            self.hide_feedback(candidate.object());
        }
        if let Some(candidate) = self.top_pull.take() {
            self.hide_feedback(candidate.object());
        }
    }

    pub(super) fn begin_gesture(
        &mut self,
        qh: &QueueHandle<Self>,
        object: TaskObjectId,
        pointer: u32,
        position: (f64, f64),
        time: u32,
    ) {
        self.abandon_gestures();
        if !self.touch_contacts.is_empty()
            || self.tablet_tools.iter().any(|tool| tool.supported && tool.surface.is_some())
        {
            return;
        }
        let Some(task) = self.tasks.get_mut(&object) else {
            return;
        };
        let Some(size) = task.logical_size else {
            return;
        };
        let immersive = task.fullscreen
            || self
                .immersion
                .as_ref()
                .is_some_and(|capture| capture.object == object);
        if immersive {
            self.swipe_back = gesture::SwipeBackCandidate::begin(
                object,
                pointer,
                position,
                size,
                gesture::EDGE_MARGIN,
                u64::from(time),
            );
        }
        if self.swipe_back.is_none() {
            self.top_pull =
                gesture::TopPullCandidate::begin(object, pointer, position, size, u64::from(time));
        }
        if (self.swipe_back.is_some() || self.top_pull.is_some())
            && task.gesture_feedback.is_none()
            && let Some(parent) = task.surface().cloned()
        {
            match gesture_feedback::Feedback::new(
                &self.layer_globals,
                &self.compositor,
                &parent,
                qh,
                task.applied_scale_120,
            ) {
                Ok(feedback) => task.gesture_feedback = Some(feedback),
                Err(error) => eprintln!("Droidloom gesture feedback: {error}"),
            }
        }
    }

    pub(super) fn motion_gesture(&mut self, id: i32, position: (f64, f64), time: u32) -> bool {
        let Ok(pointer) = u32::try_from(id) else {
            return false;
        };
        if let Some(candidate) = self
            .swipe_back
            .as_mut()
            .filter(|candidate| candidate.matches(pointer))
        {
            let update = candidate.on_motion(position, u64::from(time));
            let object = candidate.object();
            let feedback = candidate.feedback();
            let stealing = candidate.stealing();
            if update == gesture::SwipeUpdate::Cancelled {
                self.swipe_back = None;
                self.hide_feedback(object);
            } else {
                if let Some(task) = self.tasks.get_mut(&object)
                    && let Some(size) = task.logical_size
                    && let Some(indicator) = task.gesture_feedback.as_mut()
                {
                    indicator.show_side(feedback.from_left, feedback.inward, feedback.along, size);
                    if let Some(surface) = task.surface() {
                        surface.commit();
                    }
                }
                if update == gesture::SwipeUpdate::Confirmed {
                    self.swipe_back.as_mut().unwrap().mark_stealing();
                    if !stealing && let Some(contact) = self.touch_contacts.remove(&id) {
                        let _ = self.send_input(object, contact.event(TouchAction::Cancel));
                    }
                    return true;
                }
            }
        }
        if let Some(candidate) = self
            .top_pull
            .as_mut()
            .filter(|candidate| candidate.matches(pointer))
        {
            let update = candidate.on_motion(position, u64::from(time));
            let object = candidate.object();
            let pull = candidate.pull();
            if update == gesture::PullUpdate::Cancelled {
                self.top_pull = None;
                self.hide_feedback(object);
            } else if let Some(task) = self.tasks.get_mut(&object)
                && let Some(size) = task.logical_size
                && let Some(indicator) = task.gesture_feedback.as_mut()
            {
                indicator.show_top(pull, size);
                if let Some(surface) = task.surface() {
                    surface.commit();
                }
            }
        }
        false
    }

    pub(super) fn finish_gesture(&mut self, id: i32) {
        let Ok(pointer) = u32::try_from(id) else {
            return;
        };
        if self
            .swipe_back
            .as_ref()
            .is_some_and(|candidate| candidate.matches(pointer))
        {
            let mut candidate = self.swipe_back.take().unwrap();
            self.hide_feedback(candidate.object());
            if let Some(object) = candidate.on_up(0) {
                self.back_key(object);
            }
        }
        if self
            .top_pull
            .as_ref()
            .is_some_and(|candidate| candidate.matches(pointer))
        {
            let candidate = self.top_pull.take().unwrap();
            self.hide_feedback(candidate.object());
            if candidate.on_up() {
                self.reveal_controls(candidate.object());
            }
        }
    }
}
