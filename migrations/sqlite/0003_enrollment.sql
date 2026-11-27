-- Enrollment: one-time codes for the pasted install command and the device-login flow.
-- Only SHA-256 hashes of the secrets are stored. Timestamps follow 0001 (fixed-width UTC text).

CREATE TABLE enrollments (
    id              TEXT PRIMARY KEY,
    kind            TEXT NOT NULL CHECK (kind IN ('code', 'device')),
    secret_hash     TEXT NOT NULL UNIQUE,
    user_code_hash  TEXT UNIQUE,
    label           TEXT NOT NULL,
    host            TEXT NOT NULL,
    status          TEXT NOT NULL
                    CHECK (status IN ('pending', 'approved', 'consumed', 'expired', 'denied')),
    token_id        TEXT REFERENCES tokens(id),
    created_at      TEXT NOT NULL,
    expires_at      TEXT NOT NULL,
    approved_at     TEXT,
    requester_ip    TEXT
);
CREATE INDEX enrollments_status_expiry ON enrollments (status, expires_at);
