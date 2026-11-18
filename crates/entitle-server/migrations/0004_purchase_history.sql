-- Append-only history of purchase state, one row per applied state change
-- (new purchase, changed state, or owner change). Basis for revenue metrics.
--
-- `purchase_id` deliberately has no foreign key: GDPR erasure deletes the
-- customer's purchases, but their history must survive (unlinked, customer_id
-- NULL) so aggregate metrics do not change retroactively.
CREATE TABLE purchase_history (
    id                       bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    purchase_id              uuid NOT NULL,
    project_id               uuid NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    -- Owner at that time.
    customer_id              uuid REFERENCES customers(id) ON DELETE SET NULL,
    store                    text NOT NULL CHECK (store IN ('creem', 'app_store', 'play_store')),
    environment              text NOT NULL,
    store_transaction_id     text NOT NULL,
    product_id               text NOT NULL,
    kind                     text NOT NULL,
    status                   text NOT NULL,
    period_type              text NOT NULL,
    purchased_at             timestamptz NOT NULL,
    expires_at               timestamptz,
    grace_period_expires_at  timestamptz,
    auto_renew               boolean NOT NULL,
    price_amount_minor       bigint,
    price_currency           text,
    as_of                    timestamptz NOT NULL,
    -- Lifecycle events of this transition (entitle-core serde names).
    events                   text[] NOT NULL,
    -- Seeded from the state at migration time, not an observed transition.
    backfilled               boolean NOT NULL DEFAULT false,
    recorded_at              timestamptz NOT NULL DEFAULT now()
);
-- "All rows in project P between t1 and t2".
CREATE INDEX purchase_history_project_as_of ON purchase_history (project_id, as_of);
-- "State of every purchase in P as of t": DISTINCT ON (purchase_id) ... ORDER BY
-- purchase_id, as_of DESC, id DESC.
CREATE INDEX purchase_history_project_purchase ON purchase_history (project_id, purchase_id, as_of DESC, id DESC);
CREATE INDEX purchase_history_customer ON purchase_history (customer_id);

INSERT INTO purchase_history (purchase_id, project_id, customer_id, store, environment,
    store_transaction_id, product_id, kind, status, period_type, purchased_at, expires_at,
    grace_period_expires_at, auto_renew, price_amount_minor, price_currency, as_of, events,
    backfilled)
SELECT id, project_id, customer_id, store, environment, store_transaction_id, product_id, kind,
    status, period_type, purchased_at, expires_at, grace_period_expires_at, auto_renew,
    price_amount_minor, price_currency, as_of, '{}', true
FROM purchases;
