# Running Redlib on Vercel — research notes

Branch: `research/vercel`. Status: **the process boots and serves every route under
Vercel's Rust runtime protocol, verified locally** (`vercel dev`-style: process +
`VERCEL_DEV_PORT`); an actual deployment has **not** been done, because that needs a
Vercel account. Read §7 for what is still unverified.

## 1. TL;DR

Vercel's Rust runtime is not a handler API — it is:

1. start the binary with `VERCEL_DEV_PORT` and a Unix socket in `VERCEL_IPC_PATH`;
2. the binary serves HTTP on `127.0.0.1:<port>`;
3. the binary writes one NUL-terminated JSON message to that socket:
   `{"type":"server-started","payload":{"initDuration":<ms>,"httpPort":<port>}}`;
4. Vercel proxies invocations into that port, reusing the process while it is warm.

That is *exactly* what Redlib already is (a hyper HTTP server), so the port is not a
rewrite:

- `src/app.rs` — the route table + static responses, moved out of `main.rs` so the
  server binary and the function binary share one definition (no behaviour change).
- `src/bin/vercel.rs` — ~90 lines: read the port, warm the statics, serve `app()`,
  announce itself on the IPC socket.
- no special handler types, no hyper 1.x conversion, no `vercel_runtime` dependency.

Everything else (GraphQL client, token, caches, templates) is untouched.

## 2. What had to change

| Area | Change | Why |
|---|---|---|
| `src/main.rs` | route table moved to `src/app.rs` (`pub fn app(hsts)`) | the function binary must build the same routes |
| `src/bin/vercel.rs` | new: bind `127.0.0.1:$VERCEL_DEV_PORT`, serve, announce | the whole runtime protocol |
| `src/client.rs` | `serverless()` / `serverless_media_redirect()` helpers | detect Vercel by `VERCEL_DEV_PORT` |
| `src/client.rs` | 302 to the CDN for `/img`, `/vid`, `/preview`, … in serverless mode | a function response is capped at a few MiB; a video does not fit |
| `src/client.rs` | refresh the token when it went stale | a frozen process can thaw with an expired token |
| `src/oauth.rs` | `Oauth::is_stale()` (expiry instant, 2 min margin) | same |
| `src/app.rs` | CSP widened for the media hosts when redirecting | otherwise the browser blocks the redirected media |
| `src/bin/vercel.rs` | startup rate-limit check is opt-in (`REDLIB_SERVERLESS_RATE_CHECK=on`) | it is four extra requests on every cold start |

## 3. Measured on this machine (through a proxy)

| Thing | Value |
|---|---|
| cold start → first 200 on a local page (`/info`) | **581 ms** (includes the OAuth token fetch) |
| warm request, cached listing | **4–8 ms** |
| warm request, post page (one `PostComments` round trip) | **1.4 s** |
| warm request, 444-comment thread | **3.0 s** |
| media request in serverless mode | **0.7 ms** (302 to the CDN) |
| debug binary | 139 MB (with symbols) |
| release binary (`--release --bin vercel`) | **9.9 MB** |
| local release build | 1 min (cold `target/`); expect a few minutes on the first Vercel build |

The cold start is dominated by the token request; after that the process stays warm and
everything is a normal Redlib request. `maxDuration: 10` covers the 3 s worst case with
room to spare.

## 4. Things that behave differently on a serverless host

**Concurrency and identity.** Each warm instance has its own `loid` + token, so it gets
its *own* budget: the REST pool is 100/min per token and the GraphQL pool is ~600/min per
token. A burst that spins up ten instances gets ten times the quota but also ten
identities. Enable **Fluid compute** (`"fluid": true`) so one instance serves concurrent
invocations: it shares the token, the caches and the style/static responses.

**Caches.** `cached()` is per process. On a warm instance, listings and comments are
served from memory (4–8 ms above). Cold instances pay a full round trip.

**Media.** `/img`, `/vid`, `/thumb`, `/preview`, `/style`, `/static` are proxied through
the function by design, which costs both response size and bandwidth. In serverless mode
they now answer `302` to Reddit's CDN instead. The trade-off is explicit: the browser
then talks to Reddit directly, so the user's address is visible to Reddit — the same
privacy property the proxy was there to avoid. Set
`REDLIB_SERVERLESS_MEDIA_REDIRECT=off` to keep proxying and accept that media larger than
the response cap fails.

**Startup work.** `main()` forces config/instance-info/OAuth at startup and runs a
rate-limit check (4 requests). The function binary keeps the statics (so the first
invocation is not the one paying for the token) but drops the rate-limit check unless
`REDLIB_SERVERLESS_RATE_CHECK=on`.

## 5. Vercel limits that matter

| Limit | Effect here |
|---|---|
| response size ~4.5 MB | why media is redirected; also caps a very large comment page (our worst measured page is ~200 KB, fine) |
| `maxDuration` (default 10 s, up to 300 s / 800 s on paid Fluid) | worst measured request 3 s |
| memory 1 GB default | one listing page peaks around 50 MB; 512 MB is enough, 1 GB is comfortable |
| bundle size | 9.9 MB release binary — far below the Lambda limits |
| no persistent disk | Redlib keeps nothing on disk; only in-memory caches, which is fine |
| no long-lived connections in | irrelevant: Redlib is request/response only |
| per-region deployment | Reddit latency dominates; pick `fra1`/`iad1` and set `regions` explicitly |

## 6. Cost sketch

A listing page is 1 GraphQL request; a post page is 1 `PostsByIds` + 1 `PostComments`.
Warm invocations are 5–20 ms of CPU, so GB-seconds stay small; the cost driver is
invocations plus the data transferred for the HTML. With `s-maxage` on anonymous
requests (not implemented here — see §8) most of it could be CDN hits instead.

## 7. What is *not* verified

- **An actual `vercel deploy`.** The runtime protocol is implemented from
  `vercel_runtime` 2.4.1's source (`run()` + `ipc::StartMessage`), and the binary serves
  correctly on that protocol locally, but the deploy path (build image, `vercel-rust`
  runtime picking the `vercel` binary out of the crate, function routing) is untested.
- The `vercel.json` in this branch is the best guess for the current runtime:

  ```json
  {
    "functions": { "Cargo.toml": { "runtime": "vercel-rust@4.0.3", "maxDuration": 10, "memory": 1024 } },
    "rewrites": [{ "source": "/(.*)", "destination": "/Cargo.toml" }]
  }
  ```

  The older `builds` form (`{"src": "Cargo.toml", "use": "vercel-rust@4.0.3", "config": {"bin": "vercel"}}`)
  may be what the deployed runtime accepts; check what the dashboard generates.
- Whether the build image has a recent enough Rust (repo pins `rust-version = "1.81"`).
- `.vercelignore` excludes `target/` so the upload is source-only; without it the build
  upload includes a 139 MB debug tree.
- Vercel's acceptable-use terms for a public proxy instance. Redlib is a proxy in front of
  Reddit; if that is considered abuse of the free tier the deploy will not survive. Ask
  before pointing a domain at it.

## 8. If this goes further

1. `Cache-Control: public, s-maxage=60, stale-while-revalidate=300` on responses to
   requests *without* a `Cookie` header, so the CDN absorbs most traffic. Must not be
   applied to personalised responses — that is why it needs the cookie test, and why it
   is not in this branch yet.
2. Keep media redirected but rewrite `i.redd.it`/`preview.redd.it` through a small edge
   function if privacy matters more than cost.
3. `fluid: true` + `regions` tuned to the crowd the instance serves.

## 9. Honest comparison

| Host | Effort | Notes |
|---|---|---|
| **Fly.io / Railway / Render / a VPS** | **zero code change** | the existing Dockerfile runs as-is; long-lived process, one token, warm caches. This is the right answer for a real instance. |
| **Vercel (this branch)** | ~1 day of work, done | works, but media has to leave the function and the free tier is a policy risk |
| Cloudflare Workers | a rewrite | no raw sockets and hyper/rustls do not build for wasm; you would replace `src/http.rs` with `workers-rs` + `fetch` and lose the TLS-fingerprint control that this fork depends on |
| Deno Deploy | a rewrite | same problem, plus no long-lived process |

## 10. Reproducing locally

```bash
# what `vercel dev` does: start the binary on a port, then proxy to it
VERCEL_DEV_PORT=3000 cargo run --bin vercel
curl -s localhost:3000/r/rust | head -20

# without REDLIB_SERVERLESS_MEDIA_REDIRECT, media keeps streaming through the proxy
VERCEL_DEV_PORT=3000 REDLIB_SERVERLESS_MEDIA_REDIRECT=off cargo run --bin vercel
```
