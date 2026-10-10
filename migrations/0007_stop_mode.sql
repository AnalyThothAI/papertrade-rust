-- A fourth thing the operator can ask for: `stop`, close everything and then
-- end the run. SQLite cannot change a CHECK in place, so the one-row table is
-- made again with the wider list.
CREATE TABLE control_with_stop (
    id            INTEGER PRIMARY KEY CHECK (id = 1),
    mode          TEXT    NOT NULL CHECK (mode IN ('run', 'pause', 'flatten', 'stop')),
    updated_at_ms INTEGER NOT NULL
) STRICT;

INSERT INTO control_with_stop (id, mode, updated_at_ms) SELECT id, mode, updated_at_ms FROM control;
DROP TABLE control;
ALTER TABLE control_with_stop RENAME TO control;
