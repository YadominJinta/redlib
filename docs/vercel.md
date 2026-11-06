# Running Redlib on Vercel — research notes

Branch: `research/vercel`. Status: **the process boots and serves every route under
Vercel's Rust runtime protocol, verified locally** (`vercel dev`-style: process +
`VERCEL_DEV_PORT`); an actual deployment has **not** been done, because that needs a
Vercel account. Read §7 for what is still unverified.

## 1. TL;DR

Vercel's Rust runtime has **two modes**, chosen by whether the crate depends on
`vercel_runtime` (`packages/rust/src/standalone-server.ts`):

1. **Handler mode** — the crate depends on `vercel_runtime`; the binary speaks the
   runtime bridge (`run(service_fn(handler))`, requests and responses tunneled over
   IPC). This is what the `rust/hello-world` example does.
2. **Standalone mode** — the crate does *not* depend on it; the binary is treated as a
   **whole HTTP server**: it is spawned with `$PORT` and everything is proxied to it,
   with the request path preserved (`ownsRouteTable` + a `/(.*)` catch-all whose
   `request.path` transform keeps the original path).

Redlib is already a whole HTTP server, and its CLI reads `PORT` from the environment, so
the deploy target is **mode 2 with zero entry-point code**:

- no `api/index.rs`, no `vercel_runtime` dependency, no `vercel.json`;
- the builder picks the binary from `default-run = "redlib"` in `Cargo.toml`
  (`resolveStandaloneBinary`), compiles it and fronts it with its own proxy;
- the only code changes are the ones the host needs anyway: media is redirected instead
  of streamed (a function response is capped), and a frozen process refreshes an expired
  token; both keyed off `VERCEL` being set.

Getting there took three failed deploys, all documented in §7 — the interesting part is
that each error pointed at a different assumption about the runtime.

## 2. What had to change

| Area | Change | Why |
|---|---|---|
| `src/app.rs` | the route table and the static responses moved out of `main.rs` into `pub fn app(hsts)` | lets a test, a function or another entry point build the same app |
| `src/server.rs` | `Server::handle()` extracted from `listen()` | one request can be served without owning a socket; `listen` uses it too |
| `src/client.rs` | `serverless()` (Vercel/Lambda env), 302 for media, stale-token refresh | a function response is capped at a few MiB, and a frozen process wakes up with an expired token |
| `src/oauth.rs` | `Oauth::is_stale()` | same |
| `src/app.rs` | CSP widened for the media hosts when redirecting | otherwise the browser blocks the redirected media |
| `.vercelignore` | `target`, `.git` | without it the upload carries a 139 MB debug tree |

Nothing else: no entry point, no `vercel.json`, no runtime crate. The `main.rs` refactor
is a pure move, and the server binary behaves exactly as before (`cargo test --lib`: 88
tests, network ones included).

## 3. Measured on this machine (through a proxy)

| Thing | Value |
|---|---|
| cold start → first 200 on a local page (`/info`) | **574 ms** (includes the OAuth token fetch) |
| warm request, cached listing | **4–8 ms** |
| warm request, post page (one `PostComments` round trip) | **1.4 s** |
| warm request, 444-comment thread | **3.0 s** |
| media request in serverless mode | **0.7 ms** (302 to the CDN) |
| debug binary | 139 MB (with symbols) |
| release binary (`--release --bin index`) | **9.9 MB** |
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

## 7. The three failed deploys, and what each one meant

**1. `functions: { "Cargo.toml": … }`** → `[unused_function] The pattern "Cargo.toml"
defined in functions doesn't match any Serverless Functions inside the api directory`.
`functions` only *tunes* a function Vercel has already detected; it never creates one.

**2. `functions: { "api/index.rs": … }`** with a hand-written IPC binary → the same
error, which rules the pattern syntax out: no function was detected at all.

**3. No `functions` key, `vercel_runtime` added, entry at `api/index.rs`, bin `index`** →
detection worked, but the build ran `cargo build --bin main`:

```
error: no bin target named `main` in default-run packages
help: available bin targets: index, redlib
```

That one is explained by `findBinaryName()` in `packages/rust/src/lib/cargo.ts`: in
handler mode it looks up the **detected entrypoint** among the `[[bin]]` paths and, when
that fails, falls back to the file stem — and detection had picked `src/main.rs` (from
`default-run = "redlib"`), whose stem is `main`. Declaring `[[bin]] path = "src/main.rs"`
would have satisfied it, but handler mode is the wrong mode for a server: the binary is
expected to speak the IPC bridge, not to listen on a port.

**The working shape** is therefore the standalone one: drop `vercel_runtime`, drop the
`api/` entry, drop `vercel.json`. Then `resolveStandaloneMode` returns true,
`resolveStandaloneBinary` picks the `default-run` binary (`redlib` → `src/main.rs`),
`compileCargoBinary` builds it, and `createStandaloneLambda` spawns it with `$PORT`
behind the shared IPC proxy. Redlib's CLI already reads `PORT`, so it just works.

Verified locally the way that proxy runs it:

```bash
PORT=3220 VERCEL=1 ./target/debug/redlib    # standalone: one env var, no IPC
```

### Still unverified

- An actual `vercel deploy` of this shape. Everything above was derived from the builder
  source and reproduced locally with the same env the proxy uses (`$PORT`), but the
  Lambda packaging and routing are only exercised on Vercel.
- Whether `functions: {"src/main.rs": {memory / maxDuration}}` is accepted for tuning; the
  defaults (10 s, 1 GB) already cover the slowest page measured (3 s) and ~50 MB.
- Whether the build image's Rust is recent enough (repo pins `rust-version = "1.81"`).
- Vercel's acceptable-use terms for a public proxy instance.

## 8. If this goes further

1. `Cache-Control: public, s-maxage=60, stale-while-revalidate=300` on responses to
   requests *without* a `Cookie` header, so the CDN absorbs most traffic. Must not be
   applied to personalised responses — that is why it needs the cookie test, and why it
   is not in this branch yet.
2. Keep media redirected but rewrite `i.redd.it`/`preview.redd.it` through a small edge
   function if privacy matters more than cost.
3. `fluid: true` + `regions` tuned to the crowd the instance serves.

## 9. Honest comparison

See also [`serverless-platforms.md`](serverless-platforms.md) for the Cloudflare Workers / Netlify /
Cloudflare Containers comparison and the `wasm32-unknown-unknown` build evidence.

| Host | Effort | Notes |
|---|---|---|
| **Fly.io / Railway / Render / a VPS** | **zero code change** | the existing Dockerfile runs as-is; long-lived process, one token, warm caches. This is the right answer for a real instance. |
| **Vercel (this branch)** | ~1 day of work, done | works, but media has to leave the function and the free tier is a policy risk |
| Cloudflare Workers | a rewrite | no raw sockets and hyper/rustls do not build for wasm; you would replace `src/http.rs` with `workers-rs` + `fetch` and lose the TLS-fingerprint control that this fork depends on |
| Deno Deploy | a rewrite | same problem, plus no long-lived process |

## 10. Reproducing locally

```bash
# what the standalone proxy does: spawn the server on a port and forward to it
PORT=3000 VERCEL=1 cargo run

# keep media streaming through Redlib (loses the size guarantee, not the privacy)
PORT=3000 VERCEL=1 REDLIB_SERVERLESS_MEDIA_REDIRECT=off cargo run
```

Deploying is `vercel deploy` with no `vercel.json`; `.vercelignore` keeps `target/` out
of the upload.
