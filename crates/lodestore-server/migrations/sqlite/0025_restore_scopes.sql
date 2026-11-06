UPDATE api_tokens SET scopes_json = REPLACE(scopes_json, '"restore"', '"restore:download"');
