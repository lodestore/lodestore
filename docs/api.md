# REST API

The server describes its REST API as an OpenAPI 3.1 document and renders it as an interactive reference.

- `/api/docs` shows the API reference. Open it from the user menu in the UI (API reference). It needs a signed-in administrator session, like the rest of `/api/v1`.
- `/api/v1/openapi.json` is the raw OpenAPI document, with the same access rule.
- The reference works without internet access: the viewer (Scalar, MIT) is embedded in the server binary and nothing is loaded from a CDN.
- The same document is available offline with `lodestore-server openapi`.
