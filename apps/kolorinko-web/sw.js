// App-shell service worker: serve the cached CSR shell for every navigation
// (stale-while-revalidate), so real browsers bypass SSR — the wasm app boots
// from the cached `index.html` and fetches page data over WebTransport. Bots,
// the first load (before the SW controls the page), and no-JS clients fall
// through to the server's SSR. Everything else (hashed trunk assets, `/repo/`
// blobs) is left to the HTTP cache, which holds them forever (`immutable`).
//
// Release builds only — `main.rs` skips registration in debug so `trunk` edits
// aren't shadowed by a cached shell. The shell refreshes itself via
// stale-while-revalidate on every navigation, so content updates (including a
// rotated WebTransport cert hash baked into `/index.html`) propagate without a
// bump; bump `SHELL` only when the caching contract itself changes.

const SHELL = "shell-v1";
const SHELL_URL = "/index.html";

self.addEventListener("install", (e) => {
	e.waitUntil(self.skipWaiting());
});

self.addEventListener("activate", (e) => {
	e.waitUntil(
		(async () => {
			await Promise.all(
				(await caches.keys())
					.filter((k) => k !== SHELL)
					.map((k) => caches.delete(k)),
			);
			await self.clients.claim();
		})(),
	);
});

self.addEventListener("fetch", (e) => {
	if (e.request.mode !== "navigate") return;
	e.respondWith(swr(e));
});

// Stale-while-revalidate against the fixed shell URL (never the request URL —
// that would cache an SSR'd page as the shell). The first SW-served navigation
// has no cache yet, so it fetches the shell from the network and seeds the
// cache; later navigations return the cached shell instantly and refresh it in
// the background.
async function swr(e) {
	const cache = await caches.open(SHELL);
	const cached = await cache.match(SHELL_URL);
	const network = freshShell();
	e.waitUntil(
		network
			.then((r) => (r.ok ? cache.put(SHELL_URL, r.clone()) : undefined))
			.catch(() => {}),
	);
	return cached || network;
}

// Fetch the shell bypassing the HTTP cache and rebuild the Response without
// `Content-Encoding`: the browser auto-decompresses the zstd body, so storing
// the original `Content-Encoding: zstd` header in the Cache API would later
// make the browser try to decompress already-decoded bytes.
async function freshShell() {
	const r = await fetch(SHELL_URL, { cache: "reload" });
	const body = await r.text();
	return new Response(body, {
		status: r.status,
		headers: { "content-type": "text/html; charset=utf-8" },
	});
}
