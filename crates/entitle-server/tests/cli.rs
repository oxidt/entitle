mod common;

use clap::Parser;
use common::TestApp;
use entitle_server::cli::{Cli, Command, run_admin};
use serde_json::Value;
use sqlx::PgPool;

/// Serves the test app on a local port and returns its base URL.
async fn serve(app: &TestApp) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app.router.clone();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

async fn admin(url: &str, args: &[&str]) -> anyhow::Result<Value> {
    let base = ["entitle", "admin", "--url", url, "--token", common::ADMIN];
    let argv = base.iter().chain(args).copied();
    match Cli::try_parse_from(argv)?.command {
        Some(Command::Admin(a)) => run_admin(a).await,
        _ => unreachable!(),
    }
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn admin_cli_sets_up_a_project(db: PgPool) {
    let app = TestApp::new(db).await;
    let url = serve(&app).await;

    let project = admin(&url, &["projects", "create", "Quest", "--currency", "EUR"])
        .await
        .unwrap();
    assert_eq!(project["reporting_currency"], "EUR");

    // The project is addressed by name from here on.
    let key = admin(&url, &["keys", "create", "Quest"]).await.unwrap();
    assert!(key["key"].as_str().unwrap().starts_with("pk_"), "{key}");
    let ent = admin(&url, &["entitlements", "create", "Quest", "full_game"])
        .await
        .unwrap();
    assert_eq!(ent["identifier"], "full_game");
    let product = admin(
        &url,
        &[
            "products",
            "create",
            "Quest",
            "quest.beyondthe.game.full",
            "--store",
            "app_store",
            "--name",
            "Full game",
        ],
    )
    .await
    .unwrap();
    assert_eq!(product["billing_period"], Value::Null);
    admin(
        &url,
        &[
            "entitlements",
            "attach",
            "Quest",
            "full_game",
            "quest.beyondthe.game.full",
        ],
    )
    .await
    .unwrap();
    let ents = admin(&url, &["entitlements", "list", "Quest"])
        .await
        .unwrap();
    assert_eq!(ents[0]["product_ids"][0], product["id"], "{ents}");

    let dir = std::env::temp_dir().join(format!("entitle-cli-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let p8 = dir.join("AuthKey_ABC123.p8");
    std::fs::write(
        &p8,
        "-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----\n",
    )
    .unwrap();
    let saved = admin(
        &url,
        &[
            "credentials",
            "set-apple",
            "Quest",
            "--issuer-id",
            "issuer",
            "--key-id",
            "ABC123",
            "--key-file",
            p8.to_str().unwrap(),
            "--bundle-id",
            "quest.beyondthe.game",
        ],
    )
    .await
    .unwrap();
    assert_eq!(saved, "app_store credentials saved");
    std::fs::remove_dir_all(&dir).unwrap();
    let creds = admin(&url, &["credentials", "list", "Quest"])
        .await
        .unwrap();
    assert_eq!(creds[0]["store"], "app_store", "{creds}");
    let checked = admin(&url, &["credentials", "test", "Quest", "app_store"])
        .await
        .unwrap();
    assert_eq!(checked, "fake store accepted the credentials");

    // Support: grant, look up, revoke. App user ids are path-encoded.
    let user = "team a/42";
    let err = admin(&url, &["customers", "get", "Quest", user])
        .await
        .unwrap_err();
    assert!(err.to_string().starts_with("404"), "{err}");
    let info = admin(
        &url,
        &[
            "customers",
            "grant",
            "Quest",
            user,
            "full_game",
            "--days",
            "7",
        ],
    )
    .await
    .unwrap();
    assert_eq!(info["entitlements"]["full_game"]["source"], "promotional");
    assert!(info["entitlements"]["full_game"]["expires_at"].is_string());
    let info = admin(&url, &["customers", "get", "Quest", user])
        .await
        .unwrap();
    assert_eq!(info["app_user_id"], user);
    let info = admin(&url, &["customers", "revoke", "Quest", user, "full_game"])
        .await
        .unwrap();
    assert!(info["entitlements"].get("full_game").is_none(), "{info}");
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn admin_cli_reports_errors(db: PgPool) {
    let app = TestApp::new(db).await;
    let url = serve(&app).await;

    let err = admin(&url, &["keys", "list", "Nope"]).await.unwrap_err();
    assert_eq!(err.to_string(), "no project `Nope`");

    admin(&url, &["entitlements", "create", "Test", "pro"])
        .await
        .map(|_| ())
        .expect_err("duplicate entitlement");
    let err = admin(&url, &["entitlements", "attach", "Test", "pro", "missing"])
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "no product `missing`");

    let argv = [
        "entitle", "admin", "--url", &url, "--token", "wrong", "projects", "list",
    ];
    let Some(Command::Admin(args)) = Cli::try_parse_from(argv).unwrap().command else {
        unreachable!()
    };
    let err = run_admin(args).await.unwrap_err();
    assert!(err.to_string().starts_with("401"), "{err}");
}
