# Wishlist

The broad list of functionality we want in theory, not necessarily something we need immediately. Just going crazy with the list to make sure that the way we implement thing doesn't go in the way of future additions.

* Version-Control-System-like collaborative functionality.
  * It would be nice to be able to "fork" a page (or entire website!), perform edits, maybe merge the edits back.
  * Should support ratings & forumchat as well probably.
* Real-time edits.
* Simple-stupid filesystem backend.
  * I find it interesting to make follow the ideas of Obsidian's vault, making everything compatible with the plain old stupid directory of .md files.
  * This means, specifically: the ability to import website from a local directory:
    * Export website to a local directory
    * Import website from a local directory
    * Extra: Push local *updates* from a local directory, only importing changes and not overwriting the entire website.
    * Extra: at `git` backend for the website, making it possible, for example, to create a website that automatically renders GitHub repository's `main` branch.
* No URI/URL management.
  * I don't like URL management and I'm not sure I want to include any capabilities for it. That is, I'd prefer pages getting random paths the way, for example, YouTube videos do. Does it make it harder to decode the address when looking at it in a chat? Yes, but that's what previews are typically for.
* Shared space.
  * I'd prefer all the documents on the website sharing a single space, with structure on top. This is unlike Wikidot, where pages necessarily belong to a single website.
* Tags.
* WikiDot import, as in "make it possible to track a WikiDot website and manually merge in edits that are happening there".
* Forumchat.
  * I. e. a functionality for chatting about the pages, mixed in with the threading functionality of forums, that could function both as a chat and as a forum.
  * Not going to hard about it, it should not just be painful to use, not a replacement for a proper chatting platform.
  * Preferably, with the ability to reference certain lines of certain documents, being capable of "inline comments".
* Support for automatically pushing data to external tools such as webhooks, push notifications, translation, etc.
* Automatic lists. ListPages, basically.
* Rating systems for pages.
* External permission system (that guides multiple branches). With support for multiple owners with voting.

# Pluggable structure

As I've noted before, I believe that the fastest path to a deliverable is actually implementing all the functionality in Rust, and then eventually moving all this functionality to a plugin. However, nothing's stopping us from giving user a choice what pre-baked Rust plugins they want to use.

# VCS

The wiki2 test we currently have implements two axes: 1) document ids; 2) branches.
This means that for each document has different content on different branches.

But I'm pretty sure it could be hard to juggle, and this sounds like an excessively strong level of isolation. Moreover, I'm not sure if strict isolation used by systems like `git`, where mixing code from two branches is fundamentally a headache, is justified here.

Yet, it's the easiest one to implement, and is probably the most predictable and understandable one.

So the final concept is:
* There are documents. They belong to no one.
* There are branches. They belong to specific users.
* Each document may have different content over different branches.
* One can take contents of branch A and merge it into branch B, merging in all the documents.

How this plays out:
* I'm not a user of website A, that uses branch A' as main, but I want to contribute. So I create my own branch B as a fork of A', perform my edits, and suggest the owners of the branch A' to merge contents of my branch into theirs.

What are the problems of this approach:
* Since each website occupies a separate branch, it creates a little more headache when you try to link them together, but overall I feel like it's justified.

The alternatives I think'ed about and decided to un'think about:
* Per-document branches. Makes it harder to perform batch edits at no gain, it really does make sense to interpret branches as completely separate axis.
* Support for inter-document merges. Unrelated histories can't be reliably merged.
* "Overlays" instead of branches: make a single canonical version of a page, and let users create overlays. The core difference is that overlays always, automatically track all the edits of the canonical page. However, this sounds like an inconvenient default, and I'd prefer to build it on top of VCS if anyone ever needs this.
* Make "patches/edits" as units of merging instead of whole branches, just like in [Darcs](https://darcs.net/). Theoretically elegant, but probably only increases the complexity, and Darcs didn't even catch on.

# Real-time editing

Can be implemented via ephemeral branch overlays.
On top of each branch, we place a new function that aggregates real-time edits.

Unfortunately, we'll have a new data source to accompany Dentrado's core event list, and that's a painful extension of the core model, but is most likely doable.

# Closest plan

* Design mockup.
* Formalize all the text above in terms of Dentrado and update this document.
