UPDATE notification_channels
SET types_json = REPLACE(types_json, ']', ',"source_instance_gone"]')
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
  AND types_json NOT LIKE '%"source_instance_gone"%';
