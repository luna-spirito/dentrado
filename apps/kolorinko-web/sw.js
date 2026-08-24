// App-shell service worker: serve the cached CSR shell for canonical page
// navigations (stale-while-revalidate), so real browsers bypass SSR — the
// wasm app boots from the cached `/-/index.html` and fetches page data over
// WebTransport. Bots, the first load (before the SW controls the page), and
// no-JS clients fall through to the server's SSR.
//
// Only canonical `/{space}/{local}[/title]` paths get the shell. Everything
// else — slug-form paths (`/{space}/cat:name`, which the client can't
// resolve) and `/-/…` system paths — goes to the network, so the server's
// 301s and SSR still answer. The two-id shapes are checked the same way the
// server does it: exact length, strict base64url alphabet, and the marker
// bit (first bit 1 ⇒ the leading char sits in the upper half of the
// alphabet, i.e. matches /[g-z0-9_-]/).
//
// Everything else (hashed trunk assets, `/-/repo/` blobs) is left to the
// HTTP cache, which holds them forever (`immutable`).
//
// Release builds only — `main.rs` skips registration in debug so `trunk`
// edits aren't shadowed by a cached shell. The shell refreshes itself via
// stale-while-revalidate on every navigation, so content updates (including
// a rotated WebTransport cert hash baked into `/-/index.html`) propagate
// without a bump; bump `SHELL` only when the caching contract itself
// changes.

const SHELL = "shell-v1";
const SHELL_URL = "/-/index.html";

// 23-char space id / 12-char local id, 'S'/'L' marker char first.
// Canonical id shapes — mirrors kolorinko-rt ids.rs ('S'/'L' marker char +
// base64url payload). The uppercase marker is outside the slug alphabet, so
// ids and page names are syntactically disjoint.
const SPACE_RE = /^S[A-Za-z0-9_-]{22}$/;
const LOCAL_RE = /^L[A-Za-z0-9_-]{11}$/;

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
	if (!canonical(new URL(e.request.url).pathname)) return;
	e.respondWith(swr(e));
});

// `/SPACE/LOCAL` or `/SPACE/LOCAL/title`, nothing else.
function canonical(path) {
	const segs = path
		.replace(/^\/+/, "")
		.split("/")
		.filter((s) => s.length > 0);
	return (
		(segs.length === 2 || segs.length === 3) &&
		SPACE_RE.test(segs[0]) &&
		LOCAL_RE.test(segs[1])
	);
}

// Stale-while-revalidate against the fixed shell URL (never the request URL —
// that would cache an SSR'd page as the shell). The first SW-served navigation
// has no cache yet, so it fetches the shell from the network and seeds the
// cache; later navigations return the cached shell instantly and refresh it in
// the background.
async function swr(e) {
	const cache = await caches.open(SHELL);
	const cached = cache.match(SHELL_URL);
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
