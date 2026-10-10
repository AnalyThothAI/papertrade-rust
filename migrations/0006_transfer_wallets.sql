-- Funding baselines belong to the wallets that produced them, not configurable sides.
-- Old rows stay unknown: today's configuration cannot prove yesterday's owners.
ALTER TABLE transfers ADD COLUMN from_wallet TEXT;
ALTER TABLE transfers ADD COLUMN to_wallet TEXT;
