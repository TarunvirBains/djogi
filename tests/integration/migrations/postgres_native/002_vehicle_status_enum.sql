-- Postgres enum type for VehicleStatus.
-- This migration provisions the type by hand for integration testing.
CREATE TYPE vehicle_status AS ENUM ('active', 'in_maintenance', 'decommissioned');
