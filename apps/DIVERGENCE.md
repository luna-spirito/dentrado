Known & intended divergences from WikiDot:
* On `include`, WikiDot substitutes the content and then parses the result. We don't: we first parse the content & all the included modules, and then perform substitutions.
* Side-bar and top-bar are per-site, not per-URL.
* Links are automatically rewritten.
* No compatibility guarantees for broken syntax such as `[[div]] [[span]] [[/div]]`.
* Generally, we're not byte-perfect, we allow invisible HTML differences, especially where WikiDot is clearly broken.
  * `[[module CSS]]` are inline, not put into `<head>`.
  * `<tbody>`
* `[[html]]` blocks ride same-origin `srcdoc` iframes instead of WikiDot's served `/page/html/<hash>`
  route: same isolation (block `body`/`:root` CSS and scripts stay sealed in the block, exactly as on
  the live site), no route needed (static dumps would have dead ones), interiors verbatim — Wikidot's
  XHTML wrapper (`html#html-block-html` + `html-block.css` reset, inlined: no `/common--theme` route)
  with the authored document inside. Sizing differs in mechanism only: WikiDot's script shuffles
  heights through `resize-iframe.html` URLs across origins (their blocks live on a separate file
  host); ours reads `body.scrollHeight` directly and writes `frameElement.style.height`, driven by a
  `ResizeObserver`. Consequences: JS is required for height (as on WikiDot; without it an iframe
  clips), and growth that stays clipped inside an `overflow: hidden` box — which WikiDot's 250 ms
  polling eventually catches — is not observed. Width/border ride the element (`width: 100%;
  border: none`) because plain static dumps render without the base theme that owns WikiDot's
  `iframe.html-block-iframe` rule.
