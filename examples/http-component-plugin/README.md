# HTTP component demo

This sandboxed component uses the typed `http.request.v1` streaming SDK adapter
to make one unauthenticated `GET`. Its grant is restricted to HTTPS, `api.github.com`,
port 443, and the repository's commits path. It cannot represent arbitrary
headers, cookies, authorization, proxies, referers, redirects, decompression,
or private-network access.

The demo handles ordered metadata, bounded body chunks, and explicit completion,
then reports only the response status and byte count on the Touch Bar. It does
not render response content. Build it with:

```sh
cargo build --release --target wasm32-wasip2 -p touchbar-http-component-demo
```

Its deterministic replay exercises that same async stream with exact typed
fixtures and no network access:

```sh
./scripts/build-http-component-demo.sh
touchbarctl plugin replay \
  --package target/http-component-demo \
  --scenario examples/http-component-plugin/tests/replay.json
```
