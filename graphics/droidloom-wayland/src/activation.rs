//! Host-authorized activation for Android's existing task windows.
use crate::{App, TaskObjectId};
use smithay_client_toolkit::shell::WaylandSurface;
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    globals::GlobalList,
    protocol::{wl_seat::WlSeat, wl_surface::WlSurface},
};
use wayland_protocols::xdg::activation::v1::client::{
    xdg_activation_token_v1::{self, XdgActivationTokenV1},
    xdg_activation_v1::XdgActivationV1,
};

pub(super) struct Activation {
    manager: Option<XdgActivationV1>,
    input: Option<(u32, WlSeat, WlSurface, Instant)>,
    tokens: Tokens,
}

#[derive(Default)]
struct Tokens {
    mapped: BTreeSet<TaskObjectId>,
    pending: BTreeMap<TaskObjectId, String>,
    generation: u64,
    requesting: Option<TaskObjectId>,
}

impl Tokens {
    fn request(&mut self, object: TaskObjectId) -> Option<u64> {
        if self.requesting == Some(object) || self.pending.contains_key(&object) {
            return None;
        }
        self.generation = self.generation.wrapping_add(1);
        self.requesting = Some(object);
        self.pending.clear();
        Some(self.generation)
    }
    fn complete(&mut self, object: TaskObjectId, generation: u64, token: String) -> Option<String> {
        if generation != self.generation || self.requesting != Some(object) {
            return None;
        }
        self.requesting = None;
        if self.mapped.contains(&object) {
            return Some(token);
        }
        self.pending.insert(object, token);
        None
    }
    fn mapped(&mut self, object: TaskObjectId) -> Option<String> {
        self.mapped.insert(object);
        self.pending.remove(&object)
    }
    fn remove(&mut self, object: TaskObjectId) {
        if self.requesting == Some(object) {
            self.requesting = None;
            self.generation = self.generation.wrapping_add(1);
        }
        self.mapped.remove(&object);
        self.pending.remove(&object);
    }
    fn clear(&mut self) {
        *self = Self {
            generation: self.generation.wrapping_add(1),
            ..Self::default()
        };
    }
}

impl Activation {
    pub(super) fn new(globals: &GlobalList, qh: &QueueHandle<App>) -> Self {
        Self {
            manager: globals.bind(qh, 1..=1, ()).ok(),
            input: None,
            tokens: Tokens::default(),
        }
    }
    pub(super) fn supported(&self) -> bool {
        self.manager.is_some()
    }
    pub(super) fn input(&mut self, serial: u32, seat: &WlSeat, surface: &WlSurface) {
        self.input = Some((serial, seat.clone(), surface.clone(), Instant::now()));
    }
    pub(super) fn request(&mut self, qh: &QueueHandle<App>, object: TaskObjectId, app_id: &str) {
        let Some(manager) = &self.manager else {
            return;
        };
        // The explicit launcher and observer may register the same task in
        // quick succession. Keep its valid token instead of spending the
        // source input serial twice and superseding it with a denied token.
        let Some(generation) = self.tokens.request(object) else {
            return;
        };
        let token = manager.get_activation_token(qh, (object, generation));
        token.set_app_id(app_id.to_owned());
        if let Some((serial, seat, surface, at)) = &self.input
            && at.elapsed() <= Duration::from_secs(5)
            && seat.is_alive()
            && surface.is_alive()
        {
            token.set_serial(*serial, seat);
            token.set_surface(surface);
        }
        // The compositor may deny this request, including requests without a
        // recent input serial. Never manufacture focus or replay Android input.
        token.commit();
        eprintln!(
            "Droidloom requested host activation: task object={} package={app_id}",
            object.0
        );
    }
    pub(super) fn mapped(&mut self, object: TaskObjectId, surface: &WlSurface) {
        if let Some(token) = self.tokens.mapped(object)
            && let Some(manager) = &self.manager
        {
            manager.activate(token, surface);
        }
    }
    pub(super) fn remove(&mut self, object: TaskObjectId) {
        self.tokens.remove(object);
    }
    pub(super) fn clear(&mut self) {
        self.tokens.clear();
        self.input = None;
    }
}

impl Dispatch<XdgActivationTokenV1, (TaskObjectId, u64)> for App {
    fn event(
        state: &mut Self,
        proxy: &XdgActivationTokenV1,
        event: xdg_activation_token_v1::Event,
        data: &(TaskObjectId, u64),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_activation_token_v1::Event::Done { token } = event {
            proxy.destroy();
            let Some(task) = state.tasks.get(&data.0).filter(|t| t.accepts_present()) else {
                state.activation.tokens.remove(data.0);
                return;
            };
            let Some(window) = &task.window else {
                return;
            };
            if let Some(token) = state.activation.tokens.complete(data.0, data.1, token)
                && let Some(manager) = &state.activation.manager
            {
                manager.activate(token, window.wl_surface());
            }
        }
    }
}
wayland_client::delegate_noop!(App: ignore XdgActivationV1);

#[cfg(test)]
mod tests {
    use super::*;
    const A: TaskObjectId = TaskObjectId(1);
    const B: TaskObjectId = TaskObjectId(2);

    #[test]
    fn new_task_waits_for_its_first_commit_and_coalesces_duplicate_launchers() {
        let mut tokens = Tokens::default();
        let generation = tokens.request(A).unwrap();
        assert_eq!(tokens.request(A), None);
        assert_eq!(tokens.complete(A, generation, "grant".into()), None);
        assert_eq!(tokens.request(A), None);
        assert_eq!(tokens.mapped(A), Some("grant".into()));
        assert_eq!(tokens.mapped(A), None);
        let reopened = tokens.request(A).unwrap();
        assert_eq!(
            tokens.complete(A, reopened, "reopen".into()),
            Some("reopen".into())
        );
    }

    #[test]
    fn latest_destination_wins_over_late_token_replies() {
        let mut tokens = Tokens::default();
        tokens.mapped(A);
        tokens.mapped(B);
        let first = tokens.request(A).unwrap();
        let second = tokens.request(B).unwrap();
        assert_eq!(tokens.complete(A, first, "old".into()), None);
        assert_eq!(
            tokens.complete(B, second, "current".into()),
            Some("current".into())
        );
        assert_eq!(tokens.complete(B, second, "duplicate".into()), None);
    }

    #[test]
    fn closing_and_reconnecting_invalidate_outstanding_and_unmapped_tokens() {
        let mut tokens = Tokens::default();
        let closed = tokens.request(A).unwrap();
        tokens.remove(A);
        assert_eq!(tokens.complete(A, closed, "closed".into()), None);
        let pending = tokens.request(B).unwrap();
        tokens.complete(B, pending, "not-mapped".into());
        tokens.remove(B);
        assert_eq!(tokens.mapped(B), None);
        let disconnected = tokens.request(A).unwrap();
        tokens.clear();
        assert_eq!(
            tokens.complete(A, disconnected, "disconnected".into()),
            None
        );
        assert!(tokens.request(A).is_some());
    }
}
