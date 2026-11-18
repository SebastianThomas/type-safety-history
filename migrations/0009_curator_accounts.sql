CREATE TYPE curator_role AS ENUM ('viewer', 'curator', 'admin');

CREATE TABLE curator_users (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    username text NOT NULL UNIQUE CHECK (username ~ '^[a-z0-9][a-z0-9_-]{2,63}$'),
    display_name text NOT NULL CHECK (length(trim(display_name)) > 0),
    role curator_role NOT NULL DEFAULT 'curator',
    active boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE curator_api_tokens (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    user_id uuid NOT NULL REFERENCES curator_users(id) ON DELETE CASCADE,
    label text NOT NULL CHECK (length(trim(label)) > 0),
    token_prefix text NOT NULL,
    token_sha256 text NOT NULL UNIQUE CHECK (token_sha256 ~ '^[0-9a-f]{64}$'),
    expires_at timestamptz,
    last_used_at timestamptz,
    revoked_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    CHECK (expires_at IS NULL OR expires_at > created_at)
);

CREATE INDEX curator_api_tokens_active_hash_idx
    ON curator_api_tokens (token_sha256)
    WHERE revoked_at IS NULL;
