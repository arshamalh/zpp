-- Sites as announced by their gateways.
CREATE TABLE IF NOT EXISTS sites (
    site_id     TEXT PRIMARY KEY,
    info        JSONB       NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- One row per site per PTU. With TimescaleDB you would add:
--   SELECT create_hypertable('telemetry', 'ts');
-- plus a continuous aggregate for hourly/daily rollups and a retention policy.
CREATE TABLE IF NOT EXISTS telemetry (
    site_id          TEXT             NOT NULL,
    ts               TIMESTAMPTZ      NOT NULL,
    boot_id          UUID             NOT NULL,
    seq              BIGINT           NOT NULL,
    grid_kw          DOUBLE PRECISION NOT NULL,
    pv_kw            DOUBLE PRECISION NOT NULL,
    load_kw          DOUBLE PRECISION NOT NULL,
    ev_kw            DOUBLE PRECISION NOT NULL,
    ev_demand_kw     DOUBLE PRECISION NOT NULL,
    battery_kw       DOUBLE PRECISION NOT NULL,
    battery_soc_kwh  DOUBLE PRECISION,
    mode             TEXT             NOT NULL,
    schedule_id      UUID,
    received_at      TIMESTAMPTZ      NOT NULL DEFAULT now(),
    PRIMARY KEY (site_id, ts)
);

CREATE TABLE IF NOT EXISTS commands (
    command_id   UUID PRIMARY KEY,
    site_id      TEXT        NOT NULL,
    issued_at    TIMESTAMPTZ NOT NULL,
    valid_until  TIMESTAMPTZ NOT NULL,
    plan         JSONB       NOT NULL,
    state        TEXT        NOT NULL DEFAULT 'pending',
    attempts     INT         NOT NULL DEFAULT 0,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS commands_site_created ON commands (site_id, created_at DESC);
