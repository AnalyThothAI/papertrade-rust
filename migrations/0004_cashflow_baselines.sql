-- Existing rows retain their equity baselines and require conservative recovery.
-- New rows use cumulative depositedRaw/withdrawnRaw from the account stream.
ALTER TABLE transfers ADD COLUMN cashflow_baselines INTEGER NOT NULL DEFAULT 0
    CHECK (cashflow_baselines IN (0, 1));
