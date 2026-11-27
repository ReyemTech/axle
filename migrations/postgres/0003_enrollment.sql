-- Enrollment: one-time codes for the pasted install command and the device-login flow.
-- Only SHA-256 hashes of the secrets are stored.

CREATE TABLE enrollments (
    id              uuid PRIMARY KEY,
    kind            text NOT NULL CHECK (kind IN ('code', 'device')),
    secret_hash     text NOT NULL UNIQUE,
    user_code_hash  text UNIQUE,
    label           text NOT NULL,
    host            text NOT NULL,
    status          text NOT NULL
                    CHECK (status IN ('pending', 'approved', 'consumed', 'expired', 'denied')),
    token_id        uuid REFERENCES tokens(id),
    created_at      timestamptz NOT NULL,
    expires_at      timestamptz NOT NULL,
    approved_at     timestamptz,
    requester_ip    text
);
CREATE INDEX enrollments_status_expiry ON enrollments (status, expires_at);
