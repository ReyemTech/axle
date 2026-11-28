-- Quick replies (docs/superpowers/specs/2026-11-28-quick-replies-design.md), Postgres flavour.
-- Kept in step with migrations/sqlite/0008_message_options.sql.
--
-- `options` is a JSON array of 2-4 short answers an agent offers the human, or NULL. Stored
-- as text (not jsonb) so both backends read and write the same encoding.
ALTER TABLE messages ADD COLUMN options text;
