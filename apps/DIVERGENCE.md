Known & intended divergences from WikiDot:
* On `include`, WikiDot substitutes the content and then parses the result. We don't: we first parse the content & all the included modules, and then perform substitutions.
* `[[module CSS]]` are inline, not put into `<head>`.
* Links are automatically updated.
* No compatibility guarantees for broken syntax such as `[[div]] [[span]] [[/div]]`.
* Side-bar and top-bar are per-site, not per-URL.
