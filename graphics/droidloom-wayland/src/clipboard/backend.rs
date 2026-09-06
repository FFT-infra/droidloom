//! Generic Wayland clipboard protocol backends.
use super::wire::Clip;
use crate::App;
use std::{
    os::fd::BorrowedFd,
    sync::{Arc, Mutex},
};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    globals::GlobalList,
    protocol::{wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source, wl_seat},
};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1 as ed, ext_data_control_manager_v1 as em,
    ext_data_control_offer_v1 as eo, ext_data_control_source_v1 as es,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1 as wd, zwlr_data_control_manager_v1 as wm,
    zwlr_data_control_offer_v1 as wo, zwlr_data_control_source_v1 as ws,
};

#[derive(Clone, Default)]
pub struct OfferData(pub Arc<Mutex<Vec<String>>>);
#[derive(Clone)]
pub enum Offer {
    Core(wl_data_offer::WlDataOffer),
    Ext(eo::ExtDataControlOfferV1),
    Wlr(wo::ZwlrDataControlOfferV1),
}
impl Offer {
    pub fn mimes(&self) -> Vec<String> {
        let d = match self {
            Self::Core(o) => o.data::<OfferData>(),
            Self::Ext(o) => o.data::<OfferData>(),
            Self::Wlr(o) => o.data::<OfferData>(),
        };
        d.map(|d| d.0.lock().unwrap().clone()).unwrap_or_default()
    }
    pub fn receive(&self, mime: &str, fd: BorrowedFd<'_>) {
        match self {
            Self::Core(o) => o.receive(mime.into(), fd),
            Self::Ext(o) => o.receive(mime.into(), fd),
            Self::Wlr(o) => o.receive(mime.into(), fd),
        }
    }
    pub fn destroy(&self) {
        match self {
            Self::Core(o) => o.destroy(),
            Self::Ext(o) => o.destroy(),
            Self::Wlr(o) => o.destroy(),
        }
    }
}
pub enum Source {
    Core(wl_data_source::WlDataSource),
    Ext(es::ExtDataControlSourceV1),
    Wlr(ws::ZwlrDataControlSourceV1),
}
impl Source {
    pub fn destroy(&self) {
        match self {
            Self::Core(s) if s.is_alive() => s.destroy(),
            Self::Ext(s) if s.is_alive() => s.destroy(),
            Self::Wlr(s) if s.is_alive() => s.destroy(),
            _ => {}
        }
    }
}
pub struct Backend {
    core: Option<wl_data_device_manager::WlDataDeviceManager>,
    ext: Option<em::ExtDataControlManagerV1>,
    wlr: Option<wm::ZwlrDataControlManagerV1>,
    core_device: Option<wl_data_device::WlDataDevice>,
    ext_device: Option<ed::ExtDataControlDeviceV1>,
    wlr_device: Option<wd::ZwlrDataControlDeviceV1>,
}
impl Backend {
    pub fn new(globals: &GlobalList, qh: &QueueHandle<App>) -> Self {
        let ext = globals.bind(qh, 1..=1, ()).ok();
        let wlr = if ext.is_none() {
            globals.bind(qh, 1..=2, ()).ok()
        } else {
            None
        };
        let core = if ext.is_none() && wlr.is_none() {
            globals.bind(qh, 1..=3, ()).ok()
        } else {
            None
        };
        eprintln!(
            "Droidloom clipboard backend: {}",
            if ext.is_some() {
                "ext-data-control"
            } else if wlr.is_some() {
                "wlr-data-control"
            } else if core.is_some() {
                "core (keyboard focus required)"
            } else {
                "unavailable"
            }
        );
        Self {
            core,
            ext,
            wlr,
            core_device: None,
            ext_device: None,
            wlr_device: None,
        }
    }
    pub fn background(&self) -> bool {
        self.ext.is_some() || self.wlr.is_some()
    }
    pub fn seat(&mut self, seat: &wl_seat::WlSeat, qh: &QueueHandle<App>) {
        if self.core_device.is_some() || self.ext_device.is_some() || self.wlr_device.is_some() {
            return;
        }
        if let Some(m) = &self.ext {
            self.ext_device = Some(m.get_data_device(seat, qh, ()));
        } else if let Some(m) = &self.wlr {
            self.wlr_device = Some(m.get_data_device(seat, qh, ()));
        } else if let Some(m) = &self.core {
            self.core_device = Some(m.get_data_device(seat, qh, ()));
        }
    }
    pub fn publish(
        &self,
        clip: Arc<Clip>,
        serial: Option<u32>,
        qh: &QueueHandle<App>,
    ) -> Option<Source> {
        let mimes = super::mimes(&clip);
        if let (Some(m), Some(d)) = (&self.ext, &self.ext_device) {
            if clip.description.empty() {
                d.set_selection(None);
                return None;
            }
            let s = m.create_data_source(qh, clip);
            for mime in mimes {
                s.offer(mime);
            }
            d.set_selection(Some(&s));
            Some(Source::Ext(s))
        } else if let (Some(m), Some(d)) = (&self.wlr, &self.wlr_device) {
            if clip.description.empty() {
                d.set_selection(None);
                return None;
            }
            let s = m.create_data_source(qh, clip);
            for mime in mimes {
                s.offer(mime);
            }
            d.set_selection(Some(&s));
            Some(Source::Wlr(s))
        } else if let (Some(m), Some(d), Some(serial)) = (&self.core, &self.core_device, serial) {
            if clip.description.empty() {
                d.set_selection(None, serial);
                return None;
            }
            let s = m.create_data_source(qh, clip);
            for mime in mimes {
                s.offer(mime);
            }
            d.set_selection(Some(&s), serial);
            Some(Source::Core(s))
        } else {
            None
        }
    }
}
macro_rules! offer_dispatch {
    ($ty:ty,$event:path) => {
        impl Dispatch<$ty, OfferData> for App {
            fn event(
                _: &mut Self,
                _: &$ty,
                event: <$ty as Proxy>::Event,
                data: &OfferData,
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                if let $event { mime_type } = event {
                    let mut m = data.0.lock().unwrap();
                    if m.len() < 128 && mime_type.len() < 256 {
                        m.push(mime_type);
                    }
                }
            }
        }
    };
}
offer_dispatch!(wl_data_offer::WlDataOffer, wl_data_offer::Event::Offer);
offer_dispatch!(eo::ExtDataControlOfferV1, eo::Event::Offer);
offer_dispatch!(wo::ZwlrDataControlOfferV1, wo::Event::Offer);
macro_rules! source_dispatch {
    ($ty:ty,$send:path,$cancel:path) => {
        impl Dispatch<$ty, Arc<Clip>> for App {
            fn event(
                state: &mut Self,
                source: &$ty,
                event: <$ty as Proxy>::Event,
                clip: &Arc<Clip>,
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                match event {
                    $send { mime_type, fd } => {
                        state.clipboard.send_offer(clip.clone(), mime_type, fd)
                    }
                    $cancel => source.destroy(),
                    _ => {}
                }
            }
        }
    };
}
source_dispatch!(
    wl_data_source::WlDataSource,
    wl_data_source::Event::Send,
    wl_data_source::Event::Cancelled
);
source_dispatch!(
    es::ExtDataControlSourceV1,
    es::Event::Send,
    es::Event::Cancelled
);
source_dispatch!(
    ws::ZwlrDataControlSourceV1,
    ws::Event::Send,
    ws::Event::Cancelled
);
impl Dispatch<wl_data_device::WlDataDevice, ()> for App {
    fn event(
        state: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::Selection { id } => {
                state.clipboard.selection(id.map(Offer::Core))
            }
            wl_data_device::Event::Enter {
                id: Some(offer), ..
            } => offer.destroy(),
            _ => {}
        }
    }
    wayland_client::event_created_child!(App,wl_data_device::WlDataDevice,[0=>(wl_data_offer::WlDataOffer,OfferData::default())]);
}
impl Dispatch<ed::ExtDataControlDeviceV1, ()> for App {
    fn event(
        state: &mut Self,
        device: &ed::ExtDataControlDeviceV1,
        event: ed::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ed::Event::Selection { id } => state.clipboard.selection(id.map(Offer::Ext)),
            ed::Event::PrimarySelection { id: Some(o) } => o.destroy(),
            ed::Event::Finished => device.destroy(),
            _ => {}
        }
    }
    wayland_client::event_created_child!(App,ed::ExtDataControlDeviceV1,[0=>(eo::ExtDataControlOfferV1,OfferData::default())]);
}
impl Dispatch<wd::ZwlrDataControlDeviceV1, ()> for App {
    fn event(
        state: &mut Self,
        device: &wd::ZwlrDataControlDeviceV1,
        event: wd::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wd::Event::Selection { id } => state.clipboard.selection(id.map(Offer::Wlr)),
            wd::Event::PrimarySelection { id: Some(o) } => o.destroy(),
            wd::Event::Finished => device.destroy(),
            _ => {}
        }
    }
    wayland_client::event_created_child!(App,wd::ZwlrDataControlDeviceV1,[0=>(wo::ZwlrDataControlOfferV1,OfferData::default())]);
}
wayland_client::delegate_noop!(App: ignore wl_data_device_manager::WlDataDeviceManager);
wayland_client::delegate_noop!(App: ignore em::ExtDataControlManagerV1);
wayland_client::delegate_noop!(App: ignore wm::ZwlrDataControlManagerV1);
