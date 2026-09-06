use super::*;
use std::{
    io::{BufRead, BufReader},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};
use zbus::zvariant::OwnedValue;
#[derive(Debug)]
enum Observed {
    Posted(
        u32,
        u32,
        String,
        Vec<String>,
        HashMap<String, OwnedValue>,
        i32,
    ),
    Closed(u32),
}
struct Server {
    next: Arc<AtomicU32>,
    events: async_channel::Sender<Observed>,
}
#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Server {
    fn get_capabilities(&self) -> Vec<&str> {
        vec!["actions", "body", "body-markup", "icon-static"]
    }
    async fn notify(
        &self,
        _app: &str,
        replaces: u32,
        _icon: &str,
        _title: &str,
        body: &str,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> u32 {
        let id = if replaces != 0 {
            replaces
        } else {
            self.next.fetch_add(1, Ordering::SeqCst)
        };
        self.events
            .send(Observed::Posted(
                id,
                replaces,
                body.to_string(),
                actions,
                hints,
                expire_timeout,
            ))
            .await
            .unwrap();
        id
    }
    async fn close_notification(&self, id: u32) {
        self.events.send(Observed::Closed(id)).await.unwrap();
    }
}
struct Bus(std::process::Child);
impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn note(version: &str) -> Note {
    Note {
        key: "0|test.app|1".into(),
        version: version.into(),
        package: "test.app".into(),
        app: "Test".into(),
        title: "Hello".into(),
        body: "<b>text & more</b>".into(),
        actions: vec![
            Action {
                key: "default".into(),
                label: "Open".into(),
            },
            Action {
                key: "action-0".into(),
                label: "Action".into(),
            },
        ],
        urgency: 1,
        ongoing: false,
        clearable: true,
        icon: vec![127; 48 * 48 * 4],
    }
}
async fn frame(socket: &Async<UnixStream>, value: serde_json::Value) {
    let bytes = serde_json::to_vec(&value).unwrap();
    let header = (bytes.len() as u32).to_be_bytes();
    // Exercise headers and bodies split at non-record boundaries.
    for b in header {
        (&*socket).write_all(&[b]).await.unwrap();
    }
    for chunk in bytes.chunks(317) {
        (&*socket).write_all(chunk).await.unwrap();
    }
}
async fn snapshot(socket: &Async<UnixStream>, notes: &[Note]) {
    frame(socket, serde_json::json!({"type":"begin"})).await;
    for n in notes {
        frame(socket, serde_json::json!({"type":"post","notification":n})).await;
    }
    frame(socket, serde_json::json!({"type":"end"})).await;
}
async fn timeout<T>(f: impl std::future::Future<Output = T>) -> T {
    future::race(f, async {
        async_io::Timer::after(Duration::from_secs(5)).await;
        panic!("notification test timed out")
    })
    .await
}
async fn reply(socket: &Async<UnixStream>) -> serde_json::Value {
    let mut header = [0; 4];
    timeout((&*socket).read_exact(&mut header)).await.unwrap();
    let mut bytes = vec![0; u32::from_be_bytes(header) as usize];
    timeout((&*socket).read_exact(&mut bytes)).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}
async fn quiet(rx: &async_channel::Receiver<Observed>) {
    future::race(
        async { panic!("unexpected desktop event: {:?}", rx.recv().await) },
        async {
            async_io::Timer::after(Duration::from_millis(100)).await;
        },
    )
    .await
}
#[test]
fn dbus_and_android_socket_lifecycle() {
    let mut child = Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut address = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut address)
        .unwrap();
    let _bus = Bus(child);
    future::block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notification.sock");
        let listener = Async::new(UnixListener::bind(&path).unwrap()).unwrap();
        let (tx, rx) = async_channel::bounded(128);
        let server = zbus::connection::Builder::address(address.trim())
            .unwrap()
            .name(NAME)
            .unwrap()
            .serve_at(
                PATH,
                Server {
                    next: Arc::new(AtomicU32::new(100)),
                    events: tx.clone(),
                },
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        let conn = zbus::connection::Builder::address(address.trim())
            .unwrap()
            .build()
            .await
            .unwrap();
        let state = dir.path().join("state");
        let bridge = serve(&listener, &state, conn, unsafe { libc::getuid() });
        let checks = async {
            let socket = Async::<UnixStream>::connect(&path).await.unwrap();
            frame(&socket, serde_json::json!({"type":"hello","abi":1})).await;
            assert_eq!(
                reply(&socket).await,
                serde_json::json!({"type":"hello","abi":1})
            );
            snapshot(&socket, &[note("v1")]).await;
            match timeout(rx.recv()).await.unwrap() {
                Observed::Posted(100, 0, body, actions, hints, expire_timeout) => {
                    assert_eq!(expire_timeout, -1);
                    assert_eq!(body, "&lt;b&gt;text &amp; more&lt;/b&gt;");
                    assert_eq!(actions, ["default", "Open", "action-0", "Action"]);
                    assert!(hints.contains_key("image-data"));
                    assert!(bool::try_from(hints.get("suppress-sound").unwrap()).unwrap());
                }
                other => panic!("{other:?}"),
            }
            snapshot(&socket, &[note("v1")]).await;
            quiet(&rx).await;
            snapshot(&socket, &[note("v2")]).await;
            assert!(matches!(
                timeout(rx.recv()).await.unwrap(),
                Observed::Posted(100, 100, ..)
            ));
            server
                .emit_signal(
                    None::<&str>,
                    PATH,
                    NAME,
                    "ActionInvoked",
                    &(100u32, "action-0"),
                )
                .await
                .unwrap();
            let action = reply(&socket).await;
            assert_eq!(action["type"], "action");
            assert_eq!(action["version"], "v2");
            assert_eq!(action["action"], "action-0");
            // A different bus peer cannot forge notification-server actions.
            let stranger = zbus::connection::Builder::address(address.trim())
                .unwrap()
                .build()
                .await
                .unwrap();
            stranger
                .emit_signal(
                    None::<&str>,
                    PATH,
                    NAME,
                    "ActionInvoked",
                    &(100u32, "default"),
                )
                .await
                .unwrap();
            server
                .emit_signal(
                    None::<&str>,
                    PATH,
                    NAME,
                    "NotificationClosed",
                    &(100u32, 1u32),
                )
                .await
                .unwrap();
            snapshot(&socket, &[note("v2")]).await;
            quiet(&rx).await;
            snapshot(&socket, &[note("v3")]).await;
            assert!(matches!(
                timeout(rx.recv()).await.unwrap(),
                Observed::Posted(101, 0, ..)
            ));
            server
                .emit_signal(
                    None::<&str>,
                    PATH,
                    NAME,
                    "NotificationClosed",
                    &(101u32, 2u32),
                )
                .await
                .unwrap();
            let dismissal = reply(&socket).await;
            assert_eq!(dismissal["type"], "dismiss");
            assert_eq!(dismissal["version"], "v3");
            snapshot(&socket, &[]).await;
            quiet(&rx).await;
            snapshot(&socket, &[note("v4")]).await;
            assert!(matches!(
                timeout(rx.recv()).await.unwrap(),
                Observed::Posted(102, 0, ..)
            ));
            // Reconnecting Android sends a snapshot without duplicating an unchanged item.
            drop(socket);
            let socket = Async::<UnixStream>::connect(&path).await.unwrap();
            frame(&socket, serde_json::json!({"type":"hello","abi":1})).await;
            assert_eq!(
                reply(&socket).await,
                serde_json::json!({"type":"hello","abi":1})
            );
            snapshot(&socket, &[note("v4")]).await;
            quiet(&rx).await;
            snapshot(&socket, &[]).await;
            assert!(matches!(
                timeout(rx.recv()).await.unwrap(),
                Observed::Closed(102)
            ));
            snapshot(&socket, &[note("v5")]).await;
            assert!(matches!(
                timeout(rx.recv()).await.unwrap(),
                Observed::Posted(103, 0, ..)
            ));
            server.release_name(NAME).await.unwrap();
            let replacement = zbus::connection::Builder::address(address.trim())
                .unwrap()
                .name(NAME)
                .unwrap()
                .serve_at(
                    PATH,
                    Server {
                        next: Arc::new(AtomicU32::new(200)),
                        events: tx,
                    },
                )
                .unwrap()
                .build()
                .await
                .unwrap();
            assert!(matches!(
                timeout(rx.recv()).await.unwrap(),
                Observed::Posted(200, 0, ..)
            ));
            replacement
                .emit_signal(
                    None::<&str>,
                    PATH,
                    NAME,
                    "ActionInvoked",
                    &(200u32, "default"),
                )
                .await
                .unwrap();
            assert_eq!(reply(&socket).await["action"], "default");
            // Android's serial-console notice is ongoing and not clearable, but its
            // desktop popup must still use the desktop's ordinary timeout.
            let mut ongoing = note("ongoing-v6");
            ongoing.ongoing = true;
            ongoing.clearable = false;
            ongoing.urgency = 0;
            snapshot(&socket, &[ongoing.clone()]).await;
            match timeout(rx.recv()).await.unwrap() {
                Observed::Posted(200, 200, _, _, hints, expire_timeout) => {
                    assert_eq!(expire_timeout, -1);
                    assert!(bool::try_from(hints.get("resident").unwrap()).unwrap());
                }
                other => panic!("{other:?}"),
            }
            replacement
                .emit_signal(
                    None::<&str>,
                    PATH,
                    NAME,
                    "NotificationClosed",
                    &(200u32, 1u32),
                )
                .await
                .unwrap();
            snapshot(&socket, &[ongoing]).await;
            quiet(&rx).await;
            // An unrelated snapshot does not replay the expired ongoing popup.
            // Nor does expiry produce an Android cancellation command.
            let mut byte = [0];
            future::race(
                async {
                    panic!(
                        "unexpected Android command: {:?}",
                        (&socket).read(&mut byte).await
                    )
                },
                async {
                    async_io::Timer::after(Duration::from_millis(100)).await;
                },
            )
            .await;
        };
        future::race(
            async {
                bridge.await.unwrap();
                panic!("bridge stopped")
            },
            checks,
        )
        .await;
    });
}
#[test]
fn rejects_unbounded_or_invalid_frames() {
    let (a, _b) = UnixStream::pair().unwrap();
    let mut peer = Peer {
        socket: Async::new(a).unwrap(),
        buffer: vec![],
        hello: false,
        snapshot: None,
    };
    assert!(
        peer.ingest(&((MAX_FRAME + 1) as u32).to_be_bytes())
            .is_err()
    );
    let mut n = note("v1");
    assert!(n.valid());
    n.icon.push(0);
    assert!(!n.valid());
    n.icon.clear();
    n.actions[0].key = "arbitrary-intent".into();
    assert!(!n.valid());
}
