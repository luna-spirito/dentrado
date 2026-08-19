Known & intended divergences from WikiDot:
* On `include`, WikiDot substitutes the content and then parses the result. We don't: we first parse the content & all the included modules, and then perform substitutions.
* Side-bar and top-bar are per-site, not per-URL.
* Links are automatically rewritten.
* No compatibility guarantees for broken syntax such as `[[div]] [[span]] [[/div]]`.
* Generally, we're not byte-perfect, we allow invisible HTML differences, especially where WikiDot is clearly broken.
  * `[[module CSS]]` are inline, not put into `<head>`.
  * `<tbody>`
