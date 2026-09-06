//! Exercise the real clipboard client against three in-process Wayland servers.
#![allow(dead_code, clippy::all, missing_docs)]
#[path = "../src/clipboard/mod.rs"]
mod clipboard;
use std::{io::{Read,Write}, os::{fd::{AsFd,AsRawFd},unix::net::UnixStream}, sync::{Arc,Mutex,atomic::{AtomicBool,Ordering}}, time::{Duration,Instant}};
use wayland_client::{Connection,Dispatch as ClientDispatch,QueueHandle,protocol::wl_seat as client_seat};
use wayland_server::{Display,DisplayHandle,Client,Resource,Dispatch,GlobalDispatch,DataInit,New,protocol::{wl_seat,wl_data_device_manager as cm,wl_data_device as cd,wl_data_offer as co,wl_data_source as cs}};
use wayland_protocols::ext::data_control::v1::server::{ext_data_control_manager_v1 as em,ext_data_control_device_v1 as ed,ext_data_control_offer_v1 as eo,ext_data_control_source_v1 as es};
use wayland_protocols_wlr::data_control::v1::server::{zwlr_data_control_manager_v1 as wm,zwlr_data_control_device_v1 as wd,zwlr_data_control_offer_v1 as wo,zwlr_data_control_source_v1 as ws};
struct App{clipboard:clipboard::Clipboard}
impl ClientDispatch<wayland_client::protocol::wl_registry::WlRegistry,wayland_client::globals::GlobalListContents> for App {
    fn event(_: &mut Self,_:&wayland_client::protocol::wl_registry::WlRegistry,_:wayland_client::protocol::wl_registry::Event,_:&wayland_client::globals::GlobalListContents,_:&Connection,_:&QueueHandle<Self>) {}
}
impl ClientDispatch<client_seat::WlSeat,()> for App{fn event(_: &mut Self,_:&client_seat::WlSeat,_:client_seat::Event,_:&(),_:&Connection,_:&QueueHandle<Self>) {}}
struct Server{selected:Arc<Mutex<Vec<(String,String)>>>}
#[derive(Debug)]struct Peer;
impl wayland_server::backend::ClientData for Peer{}
macro_rules! global {
    ($ty:ty)=>{impl GlobalDispatch<$ty,()> for Server{
        fn bind(_: &mut Self,_:&DisplayHandle,_:&Client,new:New<$ty>,_:&(),init:&mut DataInit<'_,Self>){init.init(new,());}
    }};
}
global!(cm::WlDataDeviceManager);global!(em::ExtDataControlManagerV1);global!(wm::ZwlrDataControlManagerV1);
impl GlobalDispatch<wl_seat::WlSeat,()> for Server{
    fn bind(_: &mut Self,_:&DisplayHandle,_:&Client,new:New<wl_seat::WlSeat>,_:&(),init:&mut DataInit<'_,Self>){let seat=init.init(new,());seat.capabilities(wl_seat::Capability::Keyboard);seat.name("test".into());}
}
impl Dispatch<wl_seat::WlSeat,()> for Server{fn request(_: &mut Self,_:&Client,_:&wl_seat::WlSeat,_:wl_seat::Request,_:&(),_:&DisplayHandle,_:&mut DataInit<'_,Self>) {}}
macro_rules! manager {
    ($manager:ty,$get:path,$create:path,$device:ty,$offer:ty,$source:ty)=>{
        impl Dispatch<$manager,()> for Server{
            fn request(_: &mut Self,client:&Client,_:&$manager,request:<$manager as Resource>::Request,_:&(),dh:&DisplayHandle,init:&mut DataInit<'_,Self>){
                match request{
                    $get{id,..}=>{let device=init.init(id,());let offer=client.create_resource::<$offer,(),Self>(dh,1,()).unwrap();device.data_offer(&offer);offer.offer("text/plain;charset=utf-8".into());device.selection(Some(&offer));},
                    $create{id}=>{init.init::<$source,_>(id,());},_=>{}
                }
            }
        }
    };
}
manager!(cm::WlDataDeviceManager,cm::Request::GetDataDevice,cm::Request::CreateDataSource,cd::WlDataDevice,co::WlDataOffer,cs::WlDataSource);
manager!(em::ExtDataControlManagerV1,em::Request::GetDataDevice,em::Request::CreateDataSource,ed::ExtDataControlDeviceV1,eo::ExtDataControlOfferV1,es::ExtDataControlSourceV1);
manager!(wm::ZwlrDataControlManagerV1,wm::Request::GetDataDevice,wm::Request::CreateDataSource,wd::ZwlrDataControlDeviceV1,wo::ZwlrDataControlOfferV1,ws::ZwlrDataControlSourceV1);
macro_rules! offer {
    ($ty:ty,$receive:path)=>{impl Dispatch<$ty,()> for Server{
        fn request(_: &mut Self,_:&Client,_:&$ty,request:<$ty as Resource>::Request,_:&(),_:&DisplayHandle,_:&mut DataInit<'_,Self>){
            if let $receive{fd,..}=request{let mut out=std::fs::File::from(fd);out.write_all(b"desktop fixture").unwrap();}
        }
    }};
}
offer!(co::WlDataOffer,co::Request::Receive);offer!(eo::ExtDataControlOfferV1,eo::Request::Receive);offer!(wo::ZwlrDataControlOfferV1,wo::Request::Receive);
macro_rules! source {($ty:ty)=>{impl Dispatch<$ty,()> for Server{fn request(_: &mut Self,_:&Client,_:&$ty,_:<$ty as Resource>::Request,_:&(),_:&DisplayHandle,_:&mut DataInit<'_,Self>) {}}};}
source!(cs::WlDataSource);source!(es::ExtDataControlSourceV1);source!(ws::ZwlrDataControlSourceV1);
fn capture(state:&Server,kind:&str,send:impl FnOnce(std::os::fd::BorrowedFd<'_>)){
    let(mut input,output)=UnixStream::pair().unwrap();send(output.as_fd());drop(output);
    let selected=state.selected.clone();let kind=kind.to_string();
    std::thread::spawn(move||{input.set_read_timeout(Some(Duration::from_secs(3))).unwrap();let mut bytes=String::new();input.read_to_string(&mut bytes).unwrap();selected.lock().unwrap().push((kind,bytes));});
}
impl Dispatch<cd::WlDataDevice,()> for Server{
    fn request(state:&mut Self,_:&Client,_:&cd::WlDataDevice,request:cd::Request,_:&(),_:&DisplayHandle,_:&mut DataInit<'_,Self>){
        if let cd::Request::SetSelection{source:Some(s),serial}=request{assert_eq!(serial,42);capture(state,"core",|fd|s.send("text/plain;charset=utf-8".into(),fd));}
    }
}
macro_rules! device {
    ($ty:ty,$set:path,$name:literal)=>{impl Dispatch<$ty,()> for Server{
        fn request(state:&mut Self,_:&Client,_:&$ty,request:<$ty as Resource>::Request,_:&(),_:&DisplayHandle,_:&mut DataInit<'_,Self>){
            if let $set{source:Some(s)}=request{capture(state,$name,|fd|s.send("text/plain;charset=utf-8".into(),fd));}
        }
    }};
}
device!(ed::ExtDataControlDeviceV1,ed::Request::SetSelection,"ext");device!(wd::ZwlrDataControlDeviceV1,wd::Request::SetSelection,"wlr");
struct Stop(Arc<AtomicBool>);impl Drop for Stop{fn drop(&mut self){self.0.store(true,Ordering::Release);}}
fn run(kind:&str){
    let(client,server)=UnixStream::pair().unwrap();let mut display=Display::<Server>::new().unwrap();let mut handle=display.handle();handle.insert_client(server,Arc::new(Peer)).unwrap();
    handle.create_global::<Server,wl_seat::WlSeat,_>(7,());handle.create_global::<Server,cm::WlDataDeviceManager,_>(3,());
    if kind!="core"{handle.create_global::<Server,wm::ZwlrDataControlManagerV1,_>(2,());}
    if kind=="ext"{handle.create_global::<Server,em::ExtDataControlManagerV1,_>(1,());}
    let selected=Arc::new(Mutex::new(Vec::new()));let mut state=Server{selected:selected.clone()};let stop=Stop(Arc::new(AtomicBool::new(false)));let stopping=stop.0.clone();
    let server_thread=std::thread::spawn(move||{while !stopping.load(Ordering::Acquire){display.dispatch_clients(&mut state).unwrap();display.flush_clients().unwrap();std::thread::sleep(Duration::from_millis(1));}});
    let conn=Connection::from_socket(client).unwrap();let(globals,mut queue)=wayland_client::globals::registry_queue_init::<App>(&conn).unwrap();let qh=queue.handle();
    let dir=tempfile::tempdir().unwrap();let mut app=App{clipboard:clipboard::Clipboard::new(dir.path().join("clipboard.sock"),&globals,&qh).unwrap()};
    let seat:client_seat::WlSeat=globals.bind(&qh,1..=7,()).unwrap();app.clipboard.seat(&seat,&qh);
    let deadline=Instant::now()+Duration::from_secs(5);let mut published=false;
    while Instant::now()<deadline{
        queue.dispatch_pending(&mut app).unwrap();queue.flush().unwrap();
        if let Some(guard)=queue.prepare_read(){let mut p=libc::pollfd{fd:conn.as_fd().as_raw_fd(),events:libc::POLLIN,revents:0};unsafe{libc::poll(&raw mut p,1,5);}if p.revents!=0{guard.read().unwrap();}}
        let ready:Vec<_>=app.clipboard.fds().into_iter().map(|(fd,events)|libc::pollfd{fd,events,revents:events}).collect();app.clipboard.notify_ready(&ready);app.clipboard.pump(&qh);
        if !published&&app.clipboard.test_text()==Some("desktop fixture"){app.clipboard.test_publish("android fixture");published=true;}
        if !selected.lock().unwrap().is_empty(){break;}
    }
    assert!(published,"desktop offer never reached the bridge");assert_eq!(*selected.lock().unwrap(),vec![(kind.to_string(),"android fixture".into())]);
    drop(stop);server_thread.join().unwrap();
}
#[test]fn core_focus_and_serial_clipboard(){run("core");}
#[test]fn wlr_data_control_fallback(){run("wlr");}
#[test]fn ext_data_control_preferred_over_wlr_and_core(){run("ext");}
