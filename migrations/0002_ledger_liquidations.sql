-- A liquidated position costs its whole margin, whatever PnL its record carries;
-- the first version of this view booked every settled position as adjusted PnL
-- less the fee. The rule is the official frontend's, and the same as
-- `economics::net_result` in the bot.

DROP VIEW trades_usd;

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
    CASE kind
        WHEN 'Liquidated' THEN -CAST(margin AS REAL)
        ELSE CAST(adjusted_pnl AS REAL) - CAST(user_paid_fee AS REAL)
    END / 1e18                                                          AS net_pnl_usd,
    CAST(user_paid_fee AS REAL) / 1e18                                  AS fee_usd,
    CAST(paper_minted AS REAL) / 1e18                                   AS paper_minted
FROM trades;
