-- entitle schema. Enum-like columns are text, validated by the application
-- (entitle-core serde names) and by CHECK constraints where cheap.

CREATE TABLE projects (
    id          uuid PRIMARY KEY,
    name        text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);

-- Only SHA-256 hashes of keys are stored; the key is shown once on creation.
CREATE TABLE api_keys (
    id          uuid PRIMARY KEY,
    project_id  uuid NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    kind        text NOT NULL CHECK (kind IN ('public', 'secret')),
    key_hash    bytea NOT NULL UNIQUE,
    key_prefix  text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    revoked_at  timestamptz
);
CREATE INDEX api_keys_project ON api_keys (project_id);

-- Store credentials, sealed with XChaCha20-Poly1305 (nonce || ciphertext),
-- bound to (project_id, store) via associated data.
CREATE TABLE store_credentials (
    project_id  uuid NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    store       text NOT NULL CHECK (store IN ('creem', 'app_store', 'play_store')),
    sealed      bytea NOT NULL,
    updated_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (project_id, store)
);

CREATE TABLE products (
    id                uuid PRIMARY KEY,
    project_id        uuid NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    store             text NOT NULL CHECK (store IN ('creem', 'app_store', 'play_store')),
    store_product_id  text NOT NULL,
    display_name      text,
    created_at        timestamptz NOT NULL DEFAULT now(),
    UNIQUE (project_id, store, store_product_id)
);

CREATE TABLE entitlements (
    id          uuid PRIMARY KEY,
    project_id  uuid NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    identifier  text NOT NULL,
    description text,
    created_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (project_id, identifier)
);

CREATE TABLE entitlement_products (
    entitlement_id  uuid NOT NULL REFERENCES entitlements(id) ON DELETE CASCADE,
    product_id      uuid NOT NULL REFERENCES products(id) ON DELETE CASCADE,
    PRIMARY KEY (entitlement_id, product_id)
);

CREATE TABLE customers (
    id          uuid PRIMARY KEY,
    project_id  uuid NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE customer_aliases (
    project_id   uuid NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    app_user_id  text NOT NULL,
    customer_id  uuid NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
    created_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (project_id, app_user_id)
);
CREATE INDEX customer_aliases_customer ON customer_aliases (customer_id);

-- Account tokens handed to stores (appAccountToken, obfuscatedExternalAccountId,
-- Creem metadata). A customer's own id is its first token; merged customers'
-- tokens move to the surviving customer so late store notifications still resolve.
CREATE TABLE account_tokens (
    token        uuid PRIMARY KEY,
    customer_id  uuid NOT NULL REFERENCES customers(id) ON DELETE CASCADE
);
CREATE INDEX account_tokens_customer ON account_tokens (customer_id);

-- Latest normalized state per purchase (one row per subscription / one-time order).
CREATE TABLE purchases (
    id                       uuid PRIMARY KEY,
    project_id               uuid NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    customer_id              uuid REFERENCES customers(id) ON DELETE SET NULL,
    store                    text NOT NULL CHECK (store IN ('creem', 'app_store', 'play_store')),
    environment              text NOT NULL,
    original_transaction_id  text NOT NULL,
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
    customer_ref             uuid,
    store_customer_id        text,
    as_of                    timestamptz NOT NULL,
    created_at               timestamptz NOT NULL DEFAULT now(),
    updated_at               timestamptz NOT NULL DEFAULT now(),
    UNIQUE (project_id, store, original_transaction_id)
);
CREATE INDEX purchases_customer ON purchases (customer_id);

-- Inbound store notification ids already processed (deduplication).
CREATE TABLE store_events (
    project_id   uuid NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    store        text NOT NULL,
    event_id     text NOT NULL,
    received_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (project_id, store, event_id)
);

CREATE TABLE promotional_grants (
    id              uuid PRIMARY KEY,
    customer_id     uuid NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
    entitlement_id  uuid NOT NULL REFERENCES entitlements(id) ON DELETE CASCADE,
    granted_at      timestamptz NOT NULL DEFAULT now(),
    expires_at      timestamptz,
    revoked_at      timestamptz
);
CREATE INDEX promotional_grants_customer ON promotional_grants (customer_id);

CREATE TABLE webhook_endpoints (
    id             uuid PRIMARY KEY,
    project_id     uuid NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    url            text NOT NULL,
    sealed_secret  bytea NOT NULL,
    created_at     timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX webhook_endpoints_project ON webhook_endpoints (project_id);

-- Outgoing webhook deliveries (transactional outbox), one row per event x endpoint.
CREATE TABLE outbox (
    id               uuid PRIMARY KEY,
    endpoint_id      uuid NOT NULL REFERENCES webhook_endpoints(id) ON DELETE CASCADE,
    event_id         uuid NOT NULL,
    event_type       text NOT NULL,
    customer_id      uuid,
    payload          jsonb NOT NULL,
    status           text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'delivered', 'failed')),
    attempts         integer NOT NULL DEFAULT 0,
    next_attempt_at  timestamptz NOT NULL DEFAULT now(),
    last_error       text,
    created_at       timestamptz NOT NULL DEFAULT now(),
    delivered_at     timestamptz
);
CREATE INDEX outbox_due ON outbox (next_attempt_at) WHERE status = 'pending';
CREATE INDEX outbox_customer ON outbox (customer_id);
