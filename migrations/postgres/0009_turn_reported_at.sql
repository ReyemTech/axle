-- When the harness made the turn report the session holds, by its own clock. A report
-- older than this one is ignored, so a heartbeat carrying an old turn file cannot undo a
-- newer hook report. NULL until a client that sends report times reports.
ALTER TABLE sessions ADD COLUMN turn_reported_at timestamptz;
