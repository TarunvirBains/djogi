-- vehicles table — uses the vehicle_status Postgres enum.
-- Depends on 002_vehicle_status_enum.sql being applied first.
CREATE TABLE IF NOT EXISTS vehicles (
    id         BIGINT PRIMARY KEY DEFAULT generate_id(),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    status     vehicle_status NOT NULL
);
