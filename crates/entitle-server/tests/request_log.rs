//! Request logs name the route, never the URI: SDK paths carry anonymous ids.

mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};

use axum::http::{Method, StatusCode};
use common::TestApp;
use sqlx::PgPool;
use tracing::instrument::WithSubscriber;
use tracing_subscriber::fmt::MakeWriter;

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Captured {
    type Writer = Captured;

    fn make_writer(&'a self) -> Captured {
        self.clone()
    }
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn request_logs_record_the_route_not_the_path(db: PgPool) {
    let app = TestApp::new(db).await;
    let anon = "$entitle_anon:6f1d0c3e-2b7a-4e8f-9a1b-3c5d7e9f1a2b";
    let logs = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(logs.clone())
        .finish();
    let (status, _) = app
        .sdk(Method::GET, &format!("/customers/{anon}"), None)
        .with_subscriber(subscriber)
        .await;
    assert_eq!(status, StatusCode::OK);

    let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(
        logs.contains(r#"route="/v1/customers/{app_user_id}""#),
        "{logs}"
    );
    assert!(logs.contains("method=GET"), "{logs}");
    assert!(logs.contains("status=200"), "{logs}");
    assert!(!logs.contains("6f1d0c3e"), "{logs}");
}
