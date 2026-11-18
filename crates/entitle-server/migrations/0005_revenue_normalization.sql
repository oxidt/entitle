-- Revenue normalization: billing periods (for MRR), a per-project reporting
-- currency, and ECB euro reference rates for currency conversion.

-- ISO 8601 duration (entitle-core `BillingPeriod`). NULL = infer from the purchase.
ALTER TABLE products ADD COLUMN billing_period text
    CHECK (billing_period IN ('P1W', 'P1M', 'P2M', 'P3M', 'P6M', 'P1Y'));

ALTER TABLE projects ADD COLUMN reporting_currency text NOT NULL DEFAULT 'USD'
    CHECK (reporting_currency ~ '^[A-Z]{3}$');

-- Units of `currency` per 1 EUR on `rate_date` (ECB business days only).
CREATE TABLE fx_rates (
    rate_date  date NOT NULL,
    currency   text NOT NULL,
    per_eur    numeric NOT NULL CHECK (per_eur > 0),
    PRIMARY KEY (rate_date, currency)
);
