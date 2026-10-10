-- Transfers of USDC from one account to the other, one row each (see `strategy::transfer`).
-- A row is written before the action it describes leaves, so a restart resumes from it.
-- Balances are exact decimal strings in 18-decimal USD; the amount is whole USDC.
CREATE TABLE transfers (
    id                INTEGER PRIMARY KEY,
    started_at_ms     INTEGER NOT NULL,
    finished_at_ms    INTEGER,          -- set once it is done or abandoned
    from_side         TEXT    NOT NULL CHECK (from_side IN ('long', 'short')),
    to_side           TEXT    NOT NULL CHECK (to_side IN ('long', 'short')),
    amount_usdc       INTEGER NOT NULL CHECK (amount_usdc > 0),
    -- withdraw: the sender's wallet withdraws to its own HyperCore account
    -- send: it sends the amount on Hyperliquid to the receiver's deposit address
    -- credit: the relayer sweeps the deposit address into the receiver's balance
    -- done: the receiver holds the funds
    -- stuck: the bot will not go on by itself; `note` says why and where the funds are
    -- abandoned: the operator ended a stuck transfer
    stage             TEXT    NOT NULL CHECK (stage IN ('withdraw', 'send', 'credit', 'done', 'stuck', 'abandoned')),
    attempts          INTEGER NOT NULL, -- withdrawals signed, or sends made, in the current stage
    attempt_at_ms     INTEGER NOT NULL, -- when the latest attempt was recorded; for credit, when the stage began
    core_baseline     TEXT,             -- sender's HyperCore USDC before the latest withdrawal or send
    sender_baseline   TEXT,             -- sender's Papertrade balance before the latest withdrawal
    receiver_baseline TEXT,             -- receiver's Papertrade balance before the first send
    withdraw_intent   TEXT,             -- the latest withdrawal, see `intents`
    send_nonce        INTEGER,          -- the Hyperliquid send's nonce, the same for every attempt
    checked_at_ms     INTEGER,          -- when the relayer was last asked to check the deposit address
    note              TEXT,             -- why it is stuck, or the latest error of an action
    CHECK (from_side <> to_side),
    CHECK ((finished_at_ms IS NULL) = (stage IN ('withdraw', 'send', 'credit', 'stuck')))
) STRICT;

-- At most one transfer is under way at any time.
CREATE UNIQUE INDEX transfers_one_unfinished ON transfers ((finished_at_ms IS NULL)) WHERE finished_at_ms IS NULL;
