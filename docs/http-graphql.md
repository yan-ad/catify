# HTTP and GraphQL client contract

`cfy-api` owns the reusable asynchronous transport used by Catify commands.
It uses `reqwest` with Rustls and the Ring crypto provider, so the CLI does not
depend on a system OpenSSL installation or the heavier AWS-LC build toolchain.

## Retry safety

Retries are bounded and use exponential backoff with jitter. The default policy
allows three retries, starting at 200 milliseconds and capped at five seconds.
`Retry-After` is honored for integer delay values.

The client retries HTTP `408`, `429`, and `5xx` responses, plus connection and
timeout failures, only when replay is safe:

- `GET`, `HEAD`, `OPTIONS`, `PUT`, and `DELETE` are retryable by default.
- `POST` is not retryable by default.
- GraphQL queries are retryable.
- GraphQL mutations are not retryable unless the caller provides an
  idempotency key.
- A caller can explicitly mark a transport request as idempotent or unsafe.

## Errors and request IDs

Failures are represented by `ApiError`, which distinguishes transport errors,
HTTP status errors, GraphQL errors, malformed JSON, invalid GraphQL envelopes,
and client configuration errors. Shopify request identifiers from
`X-Request-Id` or `X-Shopify-Request-Id` are preserved on responses and errors
so diagnostics can be correlated with Shopify support.

## Secret handling

- Authentication headers must be added with `HttpClient::with_sensitive_header`.
- Debug output never includes HTTP header values, request bodies, GraphQL
  variables, or idempotency keys.
- Structured error bodies recursively redact token, password, secret,
  authorization, and access-token fields.
- Shopify token prefixes in GraphQL and HTTP error messages are redacted before
  the error leaves `cfy-api`.

Callers must still pass resulting errors through the central `cfy-cli` output
boundary, which performs environment-secret redaction before writing to stderr.
