//! Headless notification server for Android integration tests on a PRIVATE D-Bus.
use serde::Serialize;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
};
use zbus::{Connection, zvariant::OwnedValue};
const PATH: &str = "/org/freedesktop/Notifications";
#[derive(Clone, Serialize)]
struct Item {
    id: u32,
    replaces: u32,
    body: String,
    actions: Vec<String>,
    icon: bool,
    urgency: u8,
    ongoing: bool,
    expire_timeout: i32,
}
#[derive(Default)]
struct State {
    next: u32,
    items: BTreeMap<u32, Item>,
    posts: u32,
    closes: u32,
}
struct Notifications(Arc<Mutex<State>>);
struct Test(Arc<Mutex<State>>);
#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Notifications {
    fn get_capabilities(&self) -> Vec<&str> {
        vec!["actions", "body", "icon-static"]
    }
    fn get_server_information(&self) -> (&str, &str, &str, &str) {
        ("Droidloom headless test", "Droidloom", "1", "1.3")
    }
    fn notify(
        &self,
        app: &str,
        replaces: u32,
        _icon: &str,
        _summary: &str,
        body: &str,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        timeout: i32,
    ) -> u32 {
        let mut state = self.0.lock().unwrap();
        let id = if replaces == 0 {
            state.next += 1;
            state.next
        } else {
            replaces
        };
        // Do not retain or expose content from actual user applications.
        if app == "Droidloom notification test" {
            state.posts += 1;
            state.items.insert(
                id,
                Item {
                    id,
                    replaces,
                    body: body.to_string(),
                    actions,
                    icon: hints.contains_key("image-data"),
                    urgency: hints
                        .get("urgency")
                        .and_then(|v| u8::try_from(v).ok())
                        .unwrap_or(1),
                    ongoing: hints
                        .get("resident")
                        .and_then(|v| bool::try_from(v).ok())
                        .unwrap_or(false),
                    expire_timeout: timeout,
                },
            );
        }
        id
    }
    async fn close_notification(
        &self,
        id: u32,
        #[zbus(connection)] conn: &Connection,
    ) -> zbus::fdo::Result<()> {
        {
            let mut state = self.0.lock().unwrap();
            if state.items.remove(&id).is_some() {
                state.closes += 1;
            }
        }
        conn.emit_signal(
            None::<&str>,
            PATH,
            "org.freedesktop.Notifications",
            "NotificationClosed",
            &(id, 3u32),
        )
        .await
        .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }
}
#[zbus::interface(name = "org.droidloom.NotificationTest")]
impl Test {
    fn snapshot(&self) -> String {
        let s = self.0.lock().unwrap();
        serde_json::json!({"items":s.items.values().collect::<Vec<_>>(),"posts":s.posts,"closes":s.closes}).to_string()
    }
    async fn invoke(
        &self,
        id: u32,
        action: &str,
        #[zbus(connection)] conn: &Connection,
    ) -> zbus::fdo::Result<()> {
        conn.emit_signal(
            None::<&str>,
            PATH,
            "org.freedesktop.Notifications",
            "ActionInvoked",
            &(id, action),
        )
        .await
        .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }
    async fn dismiss(
        &self,
        id: u32,
        reason: u32,
        #[zbus(connection)] conn: &Connection,
    ) -> zbus::fdo::Result<()> {
        self.0.lock().unwrap().items.remove(&id);
        conn.emit_signal(
            None::<&str>,
            PATH,
            "org.freedesktop.Notifications",
            "NotificationClosed",
            &(id, reason),
        )
        .await
        .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let address = std::env::args()
        .nth(1)
        .ok_or("Pass the private test bus address explicitly")?;
    futures_lite::future::block_on(async {
        let state = Arc::new(Mutex::new(State::default()));
        let _connection = zbus::connection::Builder::address(address.as_str())?
            .name("org.freedesktop.Notifications")?
            .serve_at(PATH, Notifications(state.clone()))?
            .serve_at(PATH, Test(state))?
            .build()
            .await?;
        println!("Headless notification test server ready");
        futures_lite::future::pending::<()>().await;
        Ok(())
    })
}
