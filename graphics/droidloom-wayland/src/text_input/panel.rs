//! Optional Denial panel dismissal feedback; normal text-input-v3 stays authoritative.
mod protocol {
    #![allow(
        dead_code,
        non_camel_case_types,
        non_upper_case_globals,
        non_snake_case,
        unused_imports,
        unused_unsafe,
        unused_variables,
        clippy::all,
        clippy::pedantic,
        missing_docs
    )]
    use wayland_client;
    use wayland_protocols::wp::text_input::zv3::client::*;
    pub mod __interfaces {
        use wayland_client::backend as wayland_backend;
        use wayland_protocols::wp::text_input::zv3::client::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocol/denial-text-input-panel-v1.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("protocol/denial-text-input-panel-v1.xml");
}
use super::{App, Connection, Dispatch, QueueHandle};
use protocol::denial_text_input_panel_v1;
pub(super) use protocol::{
    denial_text_input_panel_manager_v1::DenialTextInputPanelManagerV1,
    denial_text_input_panel_v1::DenialTextInputPanelV1,
};

wayland_client::delegate_noop!(App: ignore DenialTextInputPanelManagerV1);

impl Dispatch<DenialTextInputPanelV1, ()> for App {
    fn event(
        state: &mut Self,
        _: &DenialTextInputPanelV1,
        event: denial_text_input_panel_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let denial_text_input_panel_v1::Event::Dismissed { serial } = event;
        state.text_input.dismiss(serial);
    }
}
