// App-shell service worker: serve the cached CSR shell for navigations the
// app itself can render — canonical page routes (stale-while-revalidate) —
// so real browsers bypass SSR; the wasm app boots from the cached
// `/index.html` and fetches page data over WebTransport. Bots, the first
// load (before the SW controls the page), and no-JS clients fall through
// to the server's SSR.
//
// The app-renderable paths: canonical `/{space}/{local}[/title]` pages plus
// the auxiliary `/~/…` screens (about) — their navigations get the shell
// too, and the app renders them client-side. Everything else — slug-form
// paths (`/{space}/cat:name`, which the client can't resolve), assets,
// `/-/repo/` blobs and the rest of the server-owned `/-…` namespace —
// passes through untouched: the server's 301s, SSR, standalone documents,
// and Cache-Control policies answer directly.
//
// Update contract: this script is served at `/sw.js` and the shell at
// `/index.html`, both `no-cache` — and neither URL may ever move. A moved
// script URL bricks every installed SW (the browser byte-checks the old
// address forever), and a moved shell URL turns the SWR below stale forever.
// Contents change; URLs don't. To keep a bad shell from wedging, a 404 on
// the shell revalidation evicts the cache, and a failed first fetch falls
// back to a plain network navigation (the server's SSR), so the site keeps
// working and heals on the next load.
//
// Release builds only — `main.rs` skips registration in debug so `trunk`
// edits aren't shadowed by a cached shell. The shell refreshes itself via
// stale-while-revalidate on every navigation, so content updates (including
// a rotated WebTransport cert hash baked into `/index.html`) propagate
// without a bump; bump `SHELL` only when the caching contract itself
// changes.

const SHELL = "shell-v1";
const SHELL_URL = "/index.html";

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
	if (!shellPath(new URL(e.request.url).pathname)) return;
	e.respondWith(swr(e));
});

// A navigation the app can render from the shell: `/SPACE/LOCAL[/title]`,
// or an auxiliary `/~/…` screen.
function shellPath(path) {
	if (path.startsWith("/~/")) return true;
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
// that would cache an SSR'd page as the shell). With a cached shell: serve it
// instantly, refresh in the background — and evict on a 404 (the shell
// address must exist; serving a dead shell forever is the one unrecoverable
// failure). Without one: the fetched shell answers directly, and if even
// that fetch fails, the navigation itself is re-fetched from the network so
// the server's SSR answers instead of a network error.
async function swr(e) {
	const cache = await caches.open(SHELL);
	const cached = await cache.match(SHELL_URL);
	const network = freshShell();
	e.waitUntil(
		network
			.then((r) => {
				if (r.ok) return cache.put(SHELL_URL, r.clone());
				if (r.status === 404) return cache.delete(SHELL_URL);
			})
			.catch(() => {}),
	);
	return cached || network.then((r) => (r.ok ? r : fetch(e.request)));
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
