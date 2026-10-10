-- Everything the bot knows or did, in one file an operator can open with any SQLite tool.
-- Amounts are exact decimal strings in 18-decimal USD (or PAPER); prices are wire-scale integers.

-- What the operator wants. One row.
CREATE TABLE control (
    id            INTEGER PRIMARY KEY CHECK (id = 1),
    mode          TEXT    NOT NULL CHECK (mode IN ('run', 'pause', 'flatten')),
    updated_at_ms INTEGER NOT NULL
) STRICT;

INSERT INTO control (id, mode, updated_at_ms) VALUES (1, 'run', 0);

-- What the bot currently sees and intends. One row, rewritten about once a second.
CREATE TABLE status (
    id            INTEGER PRIMARY KEY CHECK (id = 1),
    updated_at_ms INTEGER NOT NULL,
    snapshot      TEXT    NOT NULL -- JSON, see `Snapshot`
) STRICT;

-- Timeline: every change of decision, every intent, every settled position, every mode change.
CREATE TABLE events (
    id      INTEGER PRIMARY KEY,
    at_ms   INTEGER NOT NULL,
    kind    TEXT    NOT NULL, -- decision | intent | trade | mode
    summary TEXT    NOT NULL,
    detail  TEXT              -- JSON
) STRICT;

-- Write-ahead journal of signed intents. A row exists before the request leaves,
-- so a restart knows about every intent that could still execute.
CREATE TABLE intents (
    intent_id      TEXT    PRIMARY KEY,
    created_at_ms  INTEGER NOT NULL,
    side           TEXT    NOT NULL CHECK (side IN ('long', 'short')),
    action         TEXT    NOT NULL,
    deadline       INTEGER NOT NULL, -- unix seconds
    body           TEXT    NOT NULL, -- the exact request sent
    -- signed: written, outcome of sending not yet known
    -- accepted: the relayer took it in
    -- rejected: the relayer refused it; it will not execute
    -- unknown: sending failed ambiguously; it may still execute until the deadline
    -- settled: the account shows it can no longer change anything
    status         TEXT    NOT NULL CHECK (status IN ('signed', 'accepted', 'rejected', 'unknown', 'settled')),
    queue_revision TEXT,
    seen_queued    INTEGER NOT NULL DEFAULT 0,
    detail         TEXT,             -- rejection code or error
    outcome        TEXT,             -- what the relayer reported, when observed
    settled_at_ms  INTEGER
) STRICT;

CREATE INDEX intents_unresolved ON intents (status) WHERE status IN ('signed', 'accepted', 'unknown');

-- Settled positions, copied from the account's trade history.
CREATE TABLE trades (
    wallet           TEXT    NOT NULL,
    position_id      TEXT    NOT NULL,
    side             TEXT    NOT NULL CHECK (side IN ('long', 'short')),
    closed_at_ms     INTEGER NOT NULL,
    tx_hash          TEXT    NOT NULL,
    symbol           TEXT    NOT NULL,
    kind             TEXT    NOT NULL, -- PositionClosed | Liquidated
    leverage         INTEGER NOT NULL,
    margin           TEXT    NOT NULL,
    price_scale      TEXT    NOT NULL,
    entry_price      TEXT    NOT NULL,
    exit_price       TEXT    NOT NULL,
    adjusted_pnl     TEXT    NOT NULL, -- after the impact haircut, before the win fee
    user_paid_fee    TEXT    NOT NULL,
    paper_minted     TEXT    NOT NULL,
    paper_mint_basis TEXT    NOT NULL,
    PRIMARY KEY (wallet, position_id)
) STRICT;

-- The ledger in plain numbers, for display, SQL and dashboards. Floating point: not for accounting.
CREATE VIEW trades_usd AS
SELECT
    wallet,
    position_id,
    side,
    closed_at_ms,
    datetime(closed_at_ms / 1000, 'unixepoch') AS closed_at,
    symbol,
    kind,
    leverage,
    CAST(margin AS REAL) / 1e18                                         AS margin_usd,
    CAST(entry_price AS REAL) / CAST(price_scale AS REAL)               AS entry_price,
    CAST(exit_price AS REAL) / CAST(price_scale AS REAL)                AS exit_price,
    (CAST(adjusted_pnl AS REAL) - CAST(user_paid_fee AS REAL)) / 1e18   AS net_pnl_usd,
    CAST(user_paid_fee AS REAL) / 1e18                                  AS fee_usd,
    CAST(paper_minted AS REAL) / 1e18                                   AS paper_minted
FROM trades;
