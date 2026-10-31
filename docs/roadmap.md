Now, as cache is finally persistent & tons of different disjointed plans were formed and discussed, it's (hopefully) time to assemble it all together and understand the further direction.

# The basics. Again...

I'd like to first remind WHAT IS and WHAT ISN'T the core goal of the project.

Goal:
- "Archival mentality", protecting human creativity long as there are no problems with GDPR/legal matters.
  - Why: because human time and effort mean much more than storage.
- Purpose: general notekeeping, knowledge base, collaborative wiki work, data organization. Obsidian is still a major inspiration.
- Offline-enabled, always online, the Waffle House of the internet.
  - Why: Because the goal is to be the "collaborative notepad". You shouldn't need internet to open a notepad.
- Automate what's possible to automate.

Non-goals:
- Consistency: no guarantees that the page you're seeing right here right now is absolutely the latest version of the page available, it could lag.
- Tamper-proof & anticensorship: your data is controlled by the homeserver you're using. They can "silence" your, or fake your actions on the data they control.
  - RPC Authority intends to host their own instance, so this implies that admins of RPC Authority will have access to this functionality.
- "Proprietary" content & ownership: we're designed around "open source" repository paradigm, where **whoever** can make an unofficial fork of **whatever** project, the same way it's done with public repositories.
  - Again, in case of RPC Authority or whoever self-hosts this, they can make steps to "protect" them from being easy-to-copy & lock things down, but, realistically, this only harms accessibility of the content.
  - Intellectual protection laws still apply.
    - I must inform you that I'm [copyleft](https://en.wikipedia.org/wiki/Copyleft)er, so there might be a conflict of interest, but then again, Dentrado is open-source, and I am willing to implement whatever toggles are requested.

Best-effort:
- Pluggable architecture, with opt-in features.
  - Initial plan include full-blown user-plugins. After some analysis, I came to the unfortunate conclusion: no one's going to write them. So, while this partly remains, this is mostly shelved. My goal for now: make a self-contained platforms with all the features needed, and maintain it. User plugins may come later.
- Anonymity & privacy by default, with opt-in abuse prevention.
  - This is an extremely important & controversial topic, so the next section is all about this.

# Privacy vs abuse

- Anonymity & privacy is important to protect lives of real people. I don't want:
  - Personal user data to get in the hands of the hacker.
  - Someone actually getting hurt due to "trusting" their data to dentrado.art homeserver, or any other homeserver.
  - Actually, people being creatively constrained by their role. I'd really like to open the door for pseudonyms, one-shot experiences, probing creative topics that might feel embarrassing, private projects. I might be way too romantic here right now, and someone should probably put me back to the ground, but I do believe that, on the internet, nobody should know that you're a dog. I'd like to hear objections to this one, actually.
- ... but anonymity & privacy make it **much harder** to prevent abuse and deflect attacks.
  - Realistically speaking, you can't tell a modern LLM from a human apart on the web.
  - Internet giants like Meta, Reddit are extensively lobbying for "age verification" as part of the attempt to stop the bots and collect more data*.
    - \* Crazy-yet-realistic author's conspiracy theory.
  - What can be done is making attacks more costly to the attacker via additional verification layers (for example, phone number), but I'd argue going for verification is not helpful.

So, what I suggest to do:
- Keep privacy & anonymity as a soft default. Let people register **without even providing their email**, if they want so.
  - If I can avoid getting user's email, I want to avoid getting user's email, preferring things like [WebAuthn](https://webauthn.io/) instead. Email as possibility, not as an entry-level requirement.
- Multiaccounting is soft-permitted.
- Minimal logging when things are going "alright".
- Least amount of identifiable information store by default.
- No direct access to page editing, "pull request model" instead: any user can propose an update, this update must be manually accepted by an actual member/moderator.
- Full membership comes from web-of-trust instead of verification. If someone is consistently helpful (good pull-request's), or is vouched for by others, they're accepted in.
- Ultimately, no full guarantees of privacy. The [CheckUser](https://en.wikipedia.org/wiki/CheckUser) model: you're guaranteed the privacy until you're not. Deeper protection kick in when abuse is detected, *post-factum*.

Extras:
- UI facilitating multiaccounting can be disabled.
- Email verification can be forced.

# The actual roadmap

Current goal: making a feature-full Wikidot replacement.

## Auth & Identity

Login options:
- [WebAuthn](https://webauthn.io/) (recommended)
- Email

No identity & uniqueness guarantees at this exact moment, extra sybil-protection is future work.

Policy: IS data is hashed, to the point that:
* IS doesn't know how much users are exactly registered.
* IS can't recover account email.

Profiles are implemented as separate mechanism from the identity, and are stored on the actual homesever.

## Branches

- **Each and every** user can create a new branch by forking an existing one.
- By default, each domain is linked to some specific branch as the default.
- Partial forking & merging branches functionality.

Branch forking double as draft spaces/proposing changes, can be merged back into the main branch.

## Page URL & page categories

Wikidot features custom page URLs and categories. We don't. Our URLs look like:
https://rpc-wiki.dentrado.art/LAAAAAAZ59CY/Protection-Division
... where `Protection-Division`, in this example, is taken directly from the page's title.

We don't do that, but we keep the "category" as a concept for the time being, it's configured along tags.

## Permissions

Linear permission hierarchy inside each specific branch(=space), configured by the owner of the space. Each category has minimum required level to edit/post, or rate pages.

Forking a branch forks permissions & rating, but not the users linked.

Note: all the posted content is **public**, you can't make the page private/non-viewable.

## Editing

Pages are editable via regular text editor, mimicking the style of Wikidot as closely as possible.
Tag editing, file upload & other things will be probably merged into a single editing interface.

## Membership

Members must be explicitly invited to the website, for now.

# Future work

- Simple JavaScript-scripting (or scripting Rhai — TBD).
- Forum.
- Sybil-resistance via additional verification.
- Encrypted subspaces for personal notes/drafts.
