REST API of the Lodestore server. The web UI uses the same API, so everything the UI does can be scripted.

## Authentication

1. `POST /api/v1/auth/login` with `{"username": "...", "password": "..."}`. The response sets the `lodestore_session` cookie.
2. `GET /api/v1/auth/session` returns the user name and a `csrf_token`.
3. Send the cookie with every request. Requests that change data (`POST`, `PUT`, `DELETE`) also need the `X-CSRF-Token` header with that token.

```sh
URL=https://lodestore.example.com
curl -c jar -H 'Content-Type: application/json' \
  -d '{"username":"admin","password":"..."}' \
  $URL/api/v1/auth/login
TOKEN=$(curl -s -b jar $URL/api/v1/auth/session \
  | jq -r .csrf_token)
curl -b jar -H "X-CSRF-Token: $TOKEN" \
  -X POST $URL/api/v1/policies/nightly/run
```

A browser request that changes data must also come from the server's own origin (`Origin` header).

## Errors

Errors return a JSON body `{"code": "...", "message": "..."}`. `code` is stable and meant for programs; `message` is meant for people. `401` means there is no valid session, `403` with code `csrf` means the token or origin check failed, `422` means the request was understood but rejected.
