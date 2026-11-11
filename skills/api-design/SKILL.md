---
name: api-design
description: >-
  Use when designing or reviewing an interface other programs call: an HTTP or RPC API, a library's public surface, or a wire format. Covers resource and RPC conventions, errors, pagination, idempotency and versioning.
---
# API design

An interface is a promise that outlives its implementation. Design it from the caller's side, keep it small, and decide up front how it will change.

## 1. Start from use

- Write the three or four most common calls as the client would make them, before any schema. If they read awkwardly, the design is wrong.
- List what the caller must know to succeed and what can go wrong for them. Those are the inputs, outputs and errors; nothing else belongs in the interface.
- Look at what the project already exposes. A new endpoint follows the existing style even when another style is nicer.

## 2. Choose the shape

| Shape | Fits | Conventions |
|---|---|---|
| Resource (REST-like) over HTTP | Things with identity and a lifecycle: create, read, update, delete, list | Nouns in paths, verbs from HTTP methods |
| RPC (gRPC, JSON-RPC, plain POST) | Actions that are not updates of one resource: search, convert, run | Verb-noun method names, one request and one response message |
| Events (webhooks, streams) | Telling others something happened | Past-tense names, self-contained payloads, delivery at least once |

Do not mix shapes inside one area of the interface without a reason.

## 3. Resource conventions

- Paths name collections in the plural and items by identifier: `/orders`, `/orders/{id}`, `/orders/{id}/items`. Nest only for true ownership and at most two levels.
- Methods: `GET` reads and never changes state; `POST` creates or runs an action; `PUT` replaces a whole resource and is idempotent; `PATCH` changes part of one; `DELETE` removes and is idempotent.
- Status codes carry the outcome class: `200` with a body, `201` with a `Location` for a creation, `202` for accepted work that continues, `204` for success without a body, `400` malformed, `401` not authenticated, `403` not allowed, `404` no such thing (or not visible to you), `409` conflict with current state, `422` well-formed but invalid, `429` rate limited with `Retry-After`, `5xx` only for the server's own failures.
- Field names in one case style throughout. Timestamps as RFC 3339 strings in UTC. Money as an integer of minor units plus a currency code. Identifiers as opaque strings. Enumerations as strings, and clients told to tolerate unknown values.
- Never return a bare array at the top level; wrap it in an object so fields can be added.

## 4. RPC conventions

- One request message and one response message per method, even when they are empty today.
- Methods are named for what they do (`CreateInvoice`, `ListInvoices`), and standard ones behave the standard way.
- Long work returns an operation the caller can poll or subscribe to, not a held connection.
- In protobuf never reuse or renumber a field; reserve removed numbers and names. New fields are optional with a meaningful default.

## 5. Errors

Errors are part of the interface and need the same care as results.

```json
{
  "error": {
    "code": "insufficient_funds",
    "message": "The account balance is lower than the transfer amount.",
    "target": "amount",
    "details": [{ "code": "balance", "message": "Available: 12.40 EUR" }],
    "request_id": "req_8f2c"
  }
}
```

- `code` is a stable, documented, machine-readable string. Clients branch on it, never on `message`.
- `message` is for a human, says what is wrong and what to do, and leaks no internals (no stack traces, queries or host names).
- Validation errors list every invalid field in one response.
- The same condition gives the same code everywhere.
- Distinguish what the caller can fix (4xx) from what they can only retry (5xx, 429), and say whether a retry is safe.

## 6. Lists: pagination, filtering, sorting

- Every list is paginated from the first version, with a default and a maximum page size.
- Prefer cursor pagination: the response carries `next_cursor` (opaque), the request passes it back. It stays correct while data changes. Offset pagination only for small, static sets that need "jump to page".
- Ordering is stable and documented; ties are broken by a unique field.
- Filters are named query parameters with explicit semantics (`status=open`, `created_after=…`). Sorting through one parameter (`sort=-created_at`).
- Return totals only when cheap; an exact count on every page is often the slowest part.

## 7. Writes that are safe to retry

Networks fail between request and response; callers will retry.

- `PUT` and `DELETE` are idempotent by definition; make sure the implementation agrees.
- For `POST`, accept an `Idempotency-Key` header (or a client-generated identifier in the body); store the key with the result and return the stored result on a repeat.
- Concurrent updates: return a version (`ETag` or a `version` field), require it on update (`If-Match`), and answer `409` or `412` when it is stale.
- Partial updates state exactly what an absent field and a `null` field mean.

## 8. Versioning and change

- Decide what a breaking change is: removing or renaming anything, changing a type or a meaning, adding a required input, tightening validation, changing defaults or error codes.
- Additive changes (new optional inputs, new output fields, new endpoints, new enumeration values where tolerated) do not need a new version. Tell clients to ignore unknown fields.
- Put the major version where the project already does: a path prefix (`/v1/`), a header, or the package name. One scheme only.
- Deprecate before removing: mark it in the description and in responses (`Deprecation` and `Sunset` headers), give a date and the replacement, measure remaining use.
- Keep a changelog of the interface.

## 9. Security and limits

- Authentication on every call that is not deliberately public; authorisation on the object, not only the route.
- Validate all input against the schema at the boundary: types, lengths, ranges, formats. Reject unknown fields on writes if silent typos would be harmful.
- Rate limits with `429`, `Retry-After` and documented quotas. Size limits on bodies and page sizes.
- Never put secrets in URLs. Do not expose internal identifiers that reveal volume or order when that matters.

## 10. Library interfaces

The same thinking applies to a public module:

- Export the minimum. Everything public is a commitment.
- Make wrong use hard: types that cannot represent invalid states, constructors that validate, no boolean parameters whose meaning is invisible at the call site.
- Errors are typed values the caller can match on, with context.
- Follow the language's conventions for naming, ownership, iteration and async.
- Document each item with what it does, its preconditions, its errors and one example.

## 11. Write it down first

Produce the contract before the implementation: an OpenAPI document, a `.proto` file, or the public type signatures with doc comments. Review it by writing the client calls from step 1 against it. Then implement, and test the implementation against the contract (schema validation in tests, an example request and response per operation).

## Review checklist

- [ ] The common calls are short and obvious.
- [ ] Names are consistent; the same concept has one name.
- [ ] Every list is paginated; every write is safe to retry.
- [ ] Errors have stable codes and useful messages.
- [ ] Nothing internal leaks: database identifiers, column names, stack traces.
- [ ] It is clear how a field can be added without breaking clients.
- [ ] Authentication, authorisation and limits are specified.
- [ ] There is an example for each operation.
