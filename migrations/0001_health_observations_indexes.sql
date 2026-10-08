-- Migration 0001: index health_observations for the worker's hot read patterns.
--
-- Problem (ledger seq 1040, 2026-10-08): the exchange worker's
--   SELECT ... FROM health_observations WHERE resource_id=? ORDER BY observed_at DESC LIMIT n
-- queries did full table scans — 6052 rows_read to return 5 rows — because no
-- index covered (resource_id, observed_at). The */5 cron probes every lane, so
-- this burn repeats on every health cycle.
--
-- This migration adds two composite indexes:
--   1. idx_health_obs_resource_observed (resource_id, observed_at DESC)
--      Serves every "latest N observations" query (worker.js lines ~975, 1024,
--      1051, 1080, 1102, 1314, 1357, 2467): index seek on resource_id, walk
--      observed_at in order, stop after LIMIT. No temp B-tree for the sort.
--   2. idx_health_obs_resource_ok_observed (resource_id, ok, observed_at DESC)
--      Serves the health-window counts
--        WHERE resource_id=? AND ok=1 AND observed_at>?
--      (worker.js lines ~307-308, 339-340, 370, 2122) as a covering index —
--      SQLite answers without touching the table.
--
-- Measured locally (SQLite replica, 6052 rows, 8 resources):
--   BEFORE: Q1 -> SCAN health_observations + USE TEMP B-TREE FOR ORDER BY
--           Q2 -> SCAN health_observations
--   AFTER:  Q1 -> SEARCH USING INDEX idx_health_obs_resource_observed (resource_id=?)
--           Q2 -> SEARCH USING COVERING INDEX idx_health_obs_resource_ok_observed
--                 (resource_id=? AND ok=? AND observed_at>?)
-- Full proof transcript: ~/workspace/full-send/waves/wave-15/exchange-health-index-PROOF.md
--
-- Safe to apply any number of times (IF NOT EXISTS). Read-only with respect to
-- existing data: D1 builds the indexes from the table contents on apply.
-- DO NOT APPLY until Cloudflare is verified up (lane constraint, Oct 2026).

CREATE INDEX IF NOT EXISTS idx_health_obs_resource_observed
  ON health_observations (resource_id, observed_at DESC);

CREATE INDEX IF NOT EXISTS idx_health_obs_resource_ok_observed
  ON health_observations (resource_id, ok, observed_at DESC);
