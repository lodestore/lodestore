CREATE TABLE guests_unprotected (
    host_id TEXT NOT NULL,
    source_ref TEXT NOT NULL,
    since BIGINT NOT NULL,
    PRIMARY KEY (host_id, source_ref)
);

UPDATE notification_channels
SET types_json = REPLACE(types_json, ']', ',"guest_unprotected","pve_plugin_missing"]')
WHERE source = 'ui'
  AND types_json LIKE '%"backup_failed"%'
  AND types_json LIKE '%"no_recent_backup"%'
  AND types_json LIKE '%"agent_offline"%'
  AND types_json LIKE '%"check_failed"%'
  AND types_json LIKE '%"certificate_expiring"%'
  AND types_json LIKE '%"agent_update_failed"%'
  AND types_json LIKE '%"snapshot_quarantined"%'
  AND types_json LIKE '%"repository_unavailable"%'
  AND types_json LIKE '%"agent_session_conflict"%'
  AND types_json LIKE '%"agent_certificate_rejected"%'
  AND types_json LIKE '%"source_instance_gone"%'
  AND types_json LIKE '%"guarantee_downgraded"%'
  AND types_json LIKE '%"host_key_changed"%'
  AND types_json NOT LIKE '%"guest_unprotected"%'
  AND types_json NOT LIKE '%"pve_plugin_missing"%';
