# Serverless platform comparison for Redlib

Companion to [`docs/vercel.md`](vercel.md). Question: would Cloudflare Workers or Netlify be a
better host than Vercel? Short answer: **no for Workers-as-wasm, no for Netlify**, but **yes for
Cloudflare Containers** (and, on AWS, Lambda Web Adapter) — those two run the existing Docker
image with *no* code change, which beats every function-runtime option.

## 1. The measurement that decides it

Redlib's I/O layer is `tokio` + `hyper` + `hyper-rustls`/`rustls`. Workers runs WebAssembly in V8
isolates, so the question is whether that layer compiles for `wasm32-unknown-unknown`:

```
$ cargo check --target wasm32-unknown-unknown --lib
error: the wasm*-unknown-unknown targets are not supported by default, you may need to enable
       the "js" feature    (getrandom)
```

With `getrandom` patched to the `js` feature, the next wall is tokio's reactor:

```
10x cannot find `event` in `sys`          2x unresolved import `crate::sys::tcp`
 4x cannot find type `Event` in `sys`     2x cannot find `udp` in `sys`
 2x cannot find type `Selector`           2x cannot find type `Events`
 1x cannot find `IoSourceState`           1x no method named `reregister`…
```

Cloudflare's own docs confirm this is not a missing feature but the design: *"you've got to leave
your threaded async runtimes at home; meaning no Tokio or async_std support"*, and for hyper
*"`Client` requires timing dependencies which are not yet Wasm friendly"* — only the low-level
`conn` module works, over a Workers `Socket`.

So a Workers port is **not a build target, it is a rewrite of `src/http.rs` and `src/server.rs`**
onto `workers-rs` (`Fetch`, its `Request`/`Response`), with hyper kept only as an in-memory type (if
it compiles at all with `--no-default-features`) or replaced as well. Everything above that layer —
`src/app.rs` routing, the handlers, templates, `gql.rs`, OAuth logic — is portable.

There is a promising development: in September 2026 Cloudflare published an **experimental**
`wasm32-unknown-emscripten` + Tokio patchset and ran a Tokio-based server (Pumpkin) in a Durable
Object with real TCP. Pre-release; not something to depend on yet.

## 2. Platform by platform

### Cloudflare Workers (wasm)

| | |
|---|---|
| Runtime fit | **excellent**: no cold start, response body size *not* limited, no duration limit for HTTP, 64 MiB bundle cap, global edge, Cache API |
| Memory | 128 MB per isolate (cannot be raised) |
| CPU | 10 ms (free) / 30 s default, 5 min max (paid) |
| Code change | **rewrite the I/O layer** (days), lose `rustls`/TLS control (Workers `fetch` uses Cloudflare's own TLS), no background tasks → the token refresh must be lazy (this branch already made it lazy and expiry-driven) |
| Media | could stream through the Worker (no 4.5 MB cap) → no need for the CDN redirect, privacy preserved |
| Verdict | best *runtime*, worst *port cost*. Revisit when the Emscripten/Tokio work ships and if a wasm build stops needing a new HTTP client |

### Netlify

- **Functions = AWS Lambda.** Rust is *experimental* (needs `NETLIFY_EXPERIMENTAL_BUILD_RUST_SOURCE`),
  the entry point is an `aws_lambda_events::ApiGatewayProxyRequest → ApiGatewayProxyResponse`
  handler, built for `x86_64-unknown-linux-musl`. That is the same request/response mapping work as
  the Vercel branch, but on Lambda's ~6 MB response cap, so the media problem stays.
- **Edge Functions = Deno Deploy** → same wasm constraint as Workers, without workers-rs's maturity.
- Verdict: strictly worse than either Vercel or AWS; nothing to gain.

### Cloudflare Containers (beta, paid)

- Runs an ordinary **Dockerfile**, deployed with `wrangler deploy`, scale-to-zero, pay per usage.
- Instance types: `lite` 1/16 vCPU / 256 MiB … `standard-4` 4 vCPU / 12 GiB; concurrent limits are
  generous now (400 GiB RAM / 100 vCPU per account as of Sept 2025).
- **Redlib's existing `Dockerfile` runs unchanged** — same process model as a VPS, so one token,
  warm caches, media proxied, no 4.5 MB ceiling.
- Verdict: this is the "serverless + edge" answer if you want Cloudflare: zero code change.

### AWS Lambda Web Adapter (for completeness)

One line in the Dockerfile
(`COPY --from=public.ecr.aws/awsguru/aws-lambda-adapter:1.1.0 /lambda-adapter /opt/extensions/lambda-adapter`)
runs any HTTP server on Lambda, with response streaming. It makes the Vercel-style IPC work in this
branch unnecessary on AWS, but does **not** lift the response size cap.

## 3. Ranking for this repository

| Rank | Host | Code change | Media | Notes |
|---|---|---|---|---|
| 1 | Fly.io / Railway / Render / a VPS | **none** | proxied | the existing Dockerfile; one token; best cost per request |
| 2 | **Cloudflare Containers** | **none** | proxied | same as above but on CF's edge, scale-to-zero, beta + paid |
| 3 | AWS Lambda + Web Adapter / Cloud Run | **none** | capped (~6 MB) | container image, per-request billing |
| 4 | **Vercel** (this branch) | done | redirected | works; 4.5 MB cap; policy risk on the free tier |
| 5 | Netlify | ~a day, then capped | capped | no advantage over 3 or 4 |
| 6 | Cloudflare Workers (wasm) | **days**, I/O layer rewrite | proxied | best runtime, but a port; watch the Emscripten/Tokio preview |

## 4. Verified vs not

- **Verified here**: the `wasm32-unknown-unknown` build failure chain (this machine, this crate);
  the Vercel runtime protocol end-to-end locally; the reduced dependency list that a Workers port
  would have to shed (`tokio`, `hyper`, `hyper-rustls`, `rustls`, `socket2`, `mio`).
- **From documentation, not tested**: Workers limits (link below), Netlify's experimental Rust
  support, Cloudflare Containers' Dockerfile support. No account was used for any of them.

## 5. Sources

- Workers limits: <https://developers.cloudflare.com/workers/platform/limits/>
- Workers Rust / workers-rs FAQ (no tokio/async_std; hyper `Client` not wasm-friendly):
  <https://developers.cloudflare.com/workers/languages/rust/>,
  <https://github.com/cloudflare/workers-rs>
- Cloudflare Emscripten + Tokio preview: <https://blog.cloudflare.com/rust-workers-emscripten-target/>
- Cloudflare Containers: <https://developers.cloudflare.com/containers/platform/limits/>,
  <https://blog.cloudflare.com/containers-are-available-in-public-beta-for-simple-global-and-programmable/>
- Netlify Rust functions: <https://www.netlify.com/blog/2021/10/14/write-netlify-functions-in-rust/>
- AWS Lambda Web Adapter: <https://aws.github.io/aws-lambda-web-adapter/getting-started/quick-start.html>
