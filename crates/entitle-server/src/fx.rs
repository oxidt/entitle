//! ECB euro reference rates: a background task keeps `fx_rates` current, and
//! [`load`] reads them back for conversion with [`entitle_core::FxRates`].

use std::time::Duration;

use chrono::{NaiveDate, Utc};
use entitle_core::{DailyRates, FxRates, PerEur, parse_ecb_xml};
use sqlx::PgPool;
use tokio::sync::watch;

pub const ECB_DAILY_URL: &str = "https://www.ecb.europa.eu/stats/eurofxref/eurofxref-daily.xml";
pub const ECB_HIST_90D_URL: &str =
    "https://www.ecb.europa.eu/stats/eurofxref/eurofxref-hist-90d.xml";
/// The ECB publishes once per business day (~16:00 CET); polling a few times a day
/// picks the new rates up within hours whenever the server started.
const REFRESH: Duration = Duration::from_secs(6 * 3600);
const RETRY: Duration = Duration::from_secs(15 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Longer than any gap between ECB business days (weekends, Easter, Christmas).
const STALE_AFTER_DAYS: i64 = 7;

/// Inserts or updates rates (idempotent). Returns the number of rows written.
pub async fn upsert(db: &PgPool, days: &[DailyRates]) -> anyhow::Result<u64> {
    let (mut dates, mut currencies, mut rates) = (Vec::new(), Vec::new(), Vec::new());
    for day in days {
        for (currency, rate) in &day.rates {
            dates.push(day.date);
            currencies.push(currency.as_str());
            rates.push(rate.to_string());
        }
    }
    let r = sqlx::query(
        "INSERT INTO fx_rates (rate_date, currency, per_eur)
         SELECT d, c, r::numeric FROM UNNEST($1::date[], $2::text[], $3::text[]) AS t(d, c, r)
         ON CONFLICT (rate_date, currency) DO UPDATE SET per_eur = EXCLUDED.per_eur",
    )
    .bind(dates)
    .bind(currencies)
    .bind(rates)
    .execute(db)
    .await?;
    Ok(r.rows_affected())
}

/// All stored rates.
pub async fn load(db: &PgPool) -> anyhow::Result<FxRates> {
    let rows: Vec<(NaiveDate, String, String)> =
        sqlx::query_as("SELECT rate_date, currency, per_eur::text FROM fx_rates")
            .fetch_all(db)
            .await?;
    let mut fx = FxRates::default();
    for (date, currency, rate) in rows {
        let rate = PerEur::parse(&rate)
            .ok_or_else(|| anyhow::anyhow!("unparsable fx rate {currency} {date}: {rate}"))?;
        fx.insert(date, &currency, rate);
    }
    Ok(fx)
}

/// EUR and the currencies in the newest stored ECB rates, alphabetically.
pub async fn currencies(db: &PgPool) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar(
        "SELECT currency FROM fx_rates WHERE rate_date = (SELECT max(rate_date) FROM fx_rates)
         UNION SELECT 'EUR' ORDER BY 1",
    )
    .fetch_all(db)
    .await
}

/// The 90-day history on first run or after a long downtime (to fill the gap),
/// else just the latest day.
fn source_url(latest: Option<NaiveDate>, today: NaiveDate) -> &'static str {
    match latest {
        Some(d) if (today - d).num_days() <= STALE_AFTER_DAYS => ECB_DAILY_URL,
        _ => ECB_HIST_90D_URL,
    }
}

/// Fetches ECB rates and stores them.
pub async fn refresh(db: &PgPool, http: &reqwest::Client) -> anyhow::Result<()> {
    let latest: Option<NaiveDate> = sqlx::query_scalar("SELECT max(rate_date) FROM fx_rates")
        .fetch_one(db)
        .await?;
    let url = source_url(latest, Utc::now().date_naive());
    let xml = http
        .get(url)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let days = parse_ecb_xml(&xml)?;
    let rows = upsert(db, &days).await?;
    tracing::info!(url, days = days.len(), rows, "fx rates refreshed");
    Ok(())
}

/// Refreshes rates every [`REFRESH`] (retrying sooner after a failure) until
/// `shutdown` flips to `true`.
pub async fn run_worker(db: PgPool, mut shutdown: watch::Receiver<bool>) {
    let http = reqwest::Client::new();
    tracing::info!("fx rate worker started");
    loop {
        let sleep = match refresh(&db, &http).await {
            Ok(()) => REFRESH,
            Err(error) => {
                tracing::warn!(?error, "fx rate refresh failed");
                RETRY
            }
        };
        tokio::select! {
            _ = tokio::time::sleep(sleep) => {}
            _ = shutdown.changed() => break,
        }
        if *shutdown.borrow() {
            break;
        }
    }
    tracing::info!("fx rate worker stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_history_when_empty_or_stale() {
        let today: NaiveDate = "2026-09-30".parse().unwrap();
        let days_ago = |n| Some(today - chrono::Duration::days(n));
        assert_eq!(source_url(None, today), ECB_HIST_90D_URL);
        assert_eq!(source_url(days_ago(0), today), ECB_DAILY_URL);
        assert_eq!(source_url(days_ago(4), today), ECB_DAILY_URL);
        assert_eq!(source_url(days_ago(8), today), ECB_HIST_90D_URL);
    }
}
