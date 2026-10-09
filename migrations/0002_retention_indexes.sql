-- Migration 0002: receipts hot-path index (retention is enforced in code).
--
-- Burner #3 fix (wave-29, d1-burn-codefixes): billingCheck() runs
--   SELECT id,resource,cost,created_at FROM receipts ORDER BY created_at DESC LIMIT 40
-- on every */5 cron run — full receipts SCAN + temp B-tree for the sort
-- (measured wave-25). This index turns it into an index walk: no table
-- scan, no sort.
--
-- Retention policy: health_observations is append-only with no pruning; H
-- grows monotonically so every cron scan gets more expensive. Retention is
-- enforced in worker.js by pruneHealthObservations() (HEALTH_RETENTION_DAYS
-- = 30), called once per scheduled() run, using per-resource indexed
-- deletes via idx_health_obs_resource_observed from 0001 — no new index
-- needed for the prune itself.
--
-- Boot-time application path: worker.js migrate() also issues this
-- CREATE INDEX IF NOT EXISTS, so it lands automatically on next boot.
-- This file remains the out-of-band (wrangler d1 execute) application path.
--
-- Safe to apply any number of times (IF NOT EXISTS). Read-only with respect
-- to existing data. DO NOT APPLY until Cloudflare is verified up
-- (lane constraint, Oct 2026).

CREATE INDEX IF NOT EXISTS idx_receipts_created_at
  ON receipts (created_at DESC);
