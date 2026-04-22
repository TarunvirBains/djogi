-- Phase 5 Task 4: Postgres enum type for VehicleStatus.
-- DDL emission is Phase 7 work; this migration provisions the type by hand
-- for integration testing only.
CREATE TYPE vehicle_status AS ENUM ('active', 'in_maintenance', 'decommissioned');
