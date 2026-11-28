-- Quick replies (docs/superpowers/specs/2026-11-28-quick-replies-design.md), SQLite flavour.
-- Kept in step with migrations/postgres/0005_message_options.sql.
--
-- `options` is a JSON array of 2-4 short answers an agent offers the human, or NULL.
ALTER TABLE messages ADD COLUMN options TEXT;
