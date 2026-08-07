Known & intended divergences from WikiDot:
* On `include`, WikiDot substitutes the content and then parses the result. We don't: we first parse the content & all the included modules, and then perform substitutions.
* `[[module CSS]]` are inline, not put into `<head>`.
* Links are automatically updated.
