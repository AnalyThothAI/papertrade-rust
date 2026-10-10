-- A locally empty ledger is not evidence that either account has no history.
-- Keep the last successful history read and any later failure beside the rows.
CREATE TABLE ledger_sync (
    wallet          TEXT    PRIMARY KEY,
    last_success_ms INTEGER,
    attempted_at_ms INTEGER NOT NULL,
    complete        INTEGER NOT NULL CHECK (complete IN (0, 1)),
    max_age_ms      INTEGER NOT NULL CHECK (max_age_ms >= 0),
    latest_error    TEXT
) STRICT;
