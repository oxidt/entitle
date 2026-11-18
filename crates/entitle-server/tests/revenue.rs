mod common;

use axum::http::{Method, StatusCode};
use chrono::NaiveDate;
use common::TestApp;
use entitle_core::{Money, parse_ecb_xml};
use entitle_server::fx;
use serde_json::json;
use sqlx::PgPool;

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn product_billing_period(db: PgPool) {
    let app = TestApp::new(db).await;
    let products = format!("/admin/v1/projects/{}/products", app.project);

    let (s, p) = app
        .admin(
            Method::POST,
            &products,
            json!({"store": "app_store", "store_product_id": "pro_annual", "billing_period": "P1Y"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{p}");
    assert_eq!(p["billing_period"], "P1Y");
    let product = format!("{products}/{}", p["id"].as_str().unwrap());

    let (s, body) = app
        .admin(
            Method::POST,
            &products,
            json!({"store": "app_store", "store_product_id": "bad", "billing_period": "P12M"}),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");

    // Absent fields stay, null clears.
    let (s, p) = app
        .admin(
            Method::PATCH,
            &product,
            json!({"display_name": "Pro annual"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{p}");
    assert_eq!(p["display_name"], "Pro annual");
    assert_eq!(p["billing_period"], "P1Y");
    let (_, p) = app
        .admin(Method::PATCH, &product, json!({"billing_period": null}))
        .await;
    assert_eq!(p["display_name"], "Pro annual");
    assert_eq!(p["billing_period"], json!(null));
    let (_, p) = app
        .admin(
            Method::PATCH,
            &product,
            json!({"billing_period": "P1W", "display_name": null}),
        )
        .await;
    assert_eq!(p["display_name"], json!(null));
    assert_eq!(p["billing_period"], "P1W");

    let (s, _) = app
        .admin(
            Method::PATCH,
            &product,
            json!({"billing_period": "monthly"}),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = app
        .admin(
            Method::PATCH,
            &format!("{products}/{}", uuid::Uuid::new_v4()),
            json!({"billing_period": "P1M"}),
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    let (_, list) = app.admin(Method::GET, &products, json!(null)).await;
    let annual = list
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["store_product_id"] == "pro_annual")
        .unwrap();
    assert_eq!(annual["billing_period"], "P1W");
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn project_reporting_currency(db: PgPool) {
    let app = TestApp::new(db).await;
    let project = format!("/admin/v1/projects/{}", app.project);

    let (_, p) = app.admin(Method::GET, &project, json!(null)).await;
    assert_eq!(p["reporting_currency"], "USD");

    let (s, p) = app
        .admin(
            Method::PATCH,
            &project,
            json!({"reporting_currency": "eur"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{p}");
    assert_eq!(p["reporting_currency"], "EUR");
    // Setting one field leaves the other alone.
    let (_, p) = app
        .admin(
            Method::PATCH,
            &project,
            json!({"checkout_redirect_origins": ["https://app.test"]}),
        )
        .await;
    assert_eq!(p["reporting_currency"], "EUR");
    assert_eq!(p["checkout_redirect_origins"], json!(["https://app.test"]));

    for bad in ["EURO", "US", "U$D", ""] {
        let (s, _) = app
            .admin(Method::PATCH, &project, json!({"reporting_currency": bad}))
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}");
    }
}

#[sqlx::test(migrator = "entitle_server::MIGRATOR")]
async fn fx_rates_upsert_and_load(db: PgPool) {
    let hist = parse_ecb_xml(include_str!(
        "../../entitle-core/fixtures/eurofxref-hist.xml"
    ))
    .unwrap();
    let daily = parse_ecb_xml(include_str!(
        "../../entitle-core/fixtures/eurofxref-daily.xml"
    ))
    .unwrap();

    assert_eq!(fx::currencies(&db).await.unwrap(), ["EUR"]);
    assert_eq!(fx::upsert(&db, &hist).await.unwrap(), 6);
    // Idempotent; overlapping days are updated, new currencies added.
    assert_eq!(fx::upsert(&db, &hist).await.unwrap(), 6);
    assert_eq!(fx::upsert(&db, &daily).await.unwrap(), 5);
    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM fx_rates")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(count, 8);
    // The newest day's currencies, for the dashboard's picker.
    assert_eq!(
        fx::currencies(&db).await.unwrap(),
        ["EUR", "GBP", "IDR", "ISK", "JPY", "USD"]
    );

    // Exact decimals survive the numeric round trip ("136.80" as published daily).
    let (isk,): (String,) = sqlx::query_as(
        "SELECT per_eur::text FROM fx_rates WHERE currency = 'ISK' AND rate_date = '2026-09-29'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(isk, "136.80");

    let rates = fx::load(&db).await.unwrap();
    let usd = Money {
        amount_minor: 1_000_000,
        currency: "USD".into(),
    };
    let on = |d: &str| d.parse::<NaiveDate>().unwrap();
    assert_eq!(
        rates
            .convert(&usd, "EUR", on("2026-09-27"))
            .unwrap()
            .amount_minor,
        876_962
    );
    assert_eq!(
        rates
            .convert(&usd, "GBP", on("2026-09-29"))
            .unwrap()
            .amount_minor,
        754_892
    );
    assert_eq!(rates.convert(&usd, "GBP", on("2026-09-28")), None);
    assert_eq!(rates.convert(&usd, "AED", on("2026-09-29")), None);
}
