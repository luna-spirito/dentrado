This is Dentrado repository. Here, we're building a database, with one of the goals building a robust wiki platform.
It is tightly related to Fadeno: https://gitlab.com/dentrado/fadeno/fadeno-lang

---

First goal is to build a simple application that accepts a JSON file (django dump from RuFoundatio), processes it via multiple gears and returns the tree.

I believe that, right now, we should think of making of a monolith or at least something very close to it, i. e. one (or, at maximum, two) different binaries that handle all of the Dentrado and all of the Fadeno. Unfortunately, not sure how IPFS & Yggdrasil fit into this if we end up doing them.

`dentrado`, therefore, should be a CLI application.

## Evaluation
First of all, we introduce `fad eval` command for Dentrado, which reads a compiled fadeno object `.fadobj` (or takes raw `.fad`, compiles it to `.fadobj` via Haskell) and runs it. For now, we need a simple lambda calculus interpreter. It should mostly mirror `normalize` from Haskell implementation, but we could play with lazy evaluation.
Fadeno compiler currently doesn't have any serializer. So we need to implement a serializer and a deserializer.

Later, it's desirable to implement Just-in-time compilation. `cranelift`?

## Immutable vectors (RRB-Vector?) and radix maps

There are implementations for Haskell, but I'm afraid we'll have to reinvent the wheel.

## NO algebraic effects yet

Algebraic effects could greatly boost the usability of Fadeno, but that's not in our budget right now. We'll just hack our way to something that works before considering them.

## External blobs
The database will have to operate with blobs. I think that the best thing we can do is to offload blobs to IPFS, and provide a `Blob` type to Fadeno to access them.

I believe that Fadeno's `Blob` should actually only store an underlying IPFS url, with the Dentrado itself making sure that the resource is actually available and queryable (i. e. Dentrado needs to "pin" all the Blobs referenced).

## Persistent event log

We don't actually need persist everything, for now all we need is to persist event log — rest could easily live just in memory. So we need utilities to send new events to Dentrado via CLI.
Events are just Fadeno objects. Note that there needs to be a way to embed `Blob`s into them.

### Extend this to HTTP requests

We also obviously need to be able to push events through HTTP/3 or something. Authorization is likely required?

## JSON

Since Fadeno is somewhat a superset of JSON, we should make it possible to read JSON Blobs as regular Fadeno objects:

`blob_json_deserialize` : `fun {u : Int+} {A : Type+ u} Blob -> A`.

## Gear

Since we don't have algebraic effects, we'll hack impurity in.

We probably should follow the ideas `dentrado-poc` ([definition](https://gitlab.com/dentrado/dentrado/-/blob/main/src/Dentrado/POC/Memory.hs?ref_type=heads#L826-860), [utilities](https://gitlab.com/dentrado/dentrado/-/blob/main/src/Dentrado/POC/Gear.hs?ref_type=heads)), but we could even try something a little simpler:

```fadeno
/: u : Int+ -@> Cfg : Type+ u -> Out : Type+ u -> Type+ u
Gear = \Cfg Out. Record (meta : { Cache : Type+ u } \/ { initial : meta.Cache | step : Cfg -> meta.Cache -> Out })
// We could also include proof that: `self.step cfg x = self.step cfg y`
// But I don't want to think about it right now, this brings me headache.
// Maybe it could be easily proven if `Cache` type itself is generated implicitly. In general? Sound hard.

/: u : Int+ -@> Type+ u -@> Type+ u
Impure = \A. Unit -> A

/: u : Int+ -@> Cfg : Type+ u -@> Out : Type+ u -@> Cfg -> Gear Cfg Out -> Impure Out
read = IMPURE_BUILTIN
// .. that can only be executed inside of Gear context.
```
