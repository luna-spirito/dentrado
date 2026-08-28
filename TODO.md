* STOP DOUBLE-WRITES via tx_id's initiated by the client sessions?
  * Also, need to keep tx_id in case of session key routing... *eldritch sounds*

* TODO: Proper Client
* Also: consider only remapping on receiver, not on sender. IDK if that's better, maybe not, who knows :)
* TODO: WireEvent — make it an extension of StoredEvent? I don't know already.

* Приём событий
* Горизонтальное масштабирование
* Одноступенчатые gear 

НУЖНО:
* Соединить в цепочку
* Сериализация ЖУРНАЛА на диск
* Формальная верификация?..
----
* Кластер
* Истинно-распределённые запросы (может быть и не нужен)
* СТРАШНО: Сериализацию кэша Gear на диск

--------

* КЛАСТЕР ДОЛЖЕН ЧИНИТЬСЯ 
  * Более того, в рамках каждого сервера кластера должно чиниться!

* LocCtx::post_event — надо бы стереть подчистую, и разработать нормальный алгоритм тестов с полноценными клиентами СУБД.
* Refactor record handling, it's fragile as hell right now.

* Заменить placeholder с Timeline
* Нам при синхронизации разных реплик необходимо детектить tx_id. Это охренеть какой тяжёлый протокол, да.
* cmp_rga fix erroneous ordering

* event_map suspicious
* GroupKey... зачем?
* Разобраться с маршрутизаций в NetworkEvent, снести LocalCtxPostEventParams через LocalTable?
* Что за нафиг с хранением состояния Gear внутри `Core`?
* Разобраться, работает ли resolve deps, особенно для remap
* run_log_gear пугает, лучше remapp'ить до поступления в него
* AnyGearInstance разобрать, clone_cache ликвидировать

bridge.rs:
* loc_value_to_route_bytes существовать не должен
* remap_loc_value не работает правильно.
* make_unique отвратен, но я теплю.
* Localizable бы переделать, замечательно было бы

* Перепроверить адекватность secondary сейчас, доработать streaming-модель
* У нас точно там AnchorAgg/TextAgg использует LocCtx для RGA?
* DON'T LOCALIZE if WireLocCtx is empty. This should've been obvious.

---
* Вычистить vm.rs от 0-arg application.
* Recheck advanced tests
* TODO: cluster communication is currently kinda blunt, it's better to design proper async way that doesn't resent the whole "untrusted" client WireContext
* Think a little more about cross-core cross-node communication, routing, errors?

* There is a on-going `git stash` about `compio` integration. Reason for delay: we use `&mut` for certain operations with LocCtx & Core... yeeeeaaaahh....
* Remove LocValue from counter.rs
* Suggestion: have the same Core, but make a lot of different newtype wrappers for it.
* So, what's with high/low-priority channels? Get rid of separate reroute channels? What with overload? What with delays?
* Remove CoreHandle?
* And get rid of reply channel? A bad practice I feel.
* Deduplicate post_events (core<~>db)
* self-doorbell is funny. but maybe not.

* CRITICAL: decide what to do with NodeId and how to make u32 enough. Is this possible even?
* Swap Event: it has internal Rc, and I don't like that >_<
* Do we need LocMsgTypeId? It's probably obsolete by that point, overridden by the R::Group. LocGroupId merges both for quire some time already.
* CRITICAL: DEAL WITH LOCALIZABLE IN SERVER<->CLIENT INTERACTION!
* Remove `output` caching from Gears?, add foreign_ptr for shared memory?, assets-as-gears?
* CRITICAL: fix distribution in dentrado-macro, phantom group by GearId when missing?
* TODO: FATAL: TOCTOU race in `force_active`
* TODO: Crash safety (e. g. stack overflow in task), security&limits in general...
* Use separate bidi streams nstead of a single one for all the separate subscriptions...
* robots.txt... and decide with SSR+Hydration...
```
let's talk just about ServiceWorker for now.                                                                 
                                                                                                                    
 * How standard caching fits into all of this? I'm talking about web-browser's Cache-Control, ETag, etc.            
 * index.html being network-first actually sounds a little strange for responsiveness, given that we're not playing 
   on constantly redeploying?, and stale-while-revalidate for production sounds tolerable?                          
 * Let's suppose the user has installed service-worker and loads page /hello. Should it:                            
   a) Load plain old index.html and let it take the rest?                                                           
   b) Actually perform request to /hello, letting the SSR do its job?                                               
 * Do we need to remake our assets handling in a way that all the URLs are content-addressed? Sounds preferrable.   
                                                                                                                    
 My take on all of this:                                                                                            
 * All assets are to be remade to be content-addressed (this introduces extra overhead on the server part, but      
   probably worth it?)                                                                                              
 * Cache-Control: aggressive for content-addressed assets (such as .wasm, media). No ETag functionality needed. No  
   Cache-Control for /index.html and /hello (and other pages), no ETag.                                             
 * If service worker is installed, it forces all the page requests to serve empty cached index.html,                
   stale-while-revalidate in production, network-first in development.                                              
 * If service worker is not installed, raw /hello (or whatever) request hits the server, and it serves SSR.         
```
* Разобраться с subscribe_wire & to_wire_out наконец
* incremental diff_changes
* `/repo` -- страшный endpoint.
* alt≠hash
* Assume append-only for git?
* Timer shouldn't resend on nothing-changed?
* Client keep-alive? No keep-alive right now, he connection probably gets dropped after idle.
* compression over WebTransport? Well, that's problematic...
* limit panics...
* DataId → 16 bytes? Probably not.
* zstd-over-wire?
* URL substitutions works horribly. Maybe... sw.js substitution?
* article_latest urgently needs caching... and a good one... it makes way too much queries, by the way, doesn't it?

* Title autofix? Capitalized title? Also, `/` has weird TITLE
* `fetch` route
* TODO: should core_event_loop await on processed messages? THIS IS A BOTTLENECK.
* Bottlenecks
* INTER for other fonts

-----

## Parser: brackets split across `[[include]]` boundaries

Wikidot assembles includes textually *before* parsing, so an included
component may leave `[[div]]`/`[[cell]]`/`[[collapsible]]` open for the
includer to close. Kolorinko parses each page separately and tree-splices,
so both halves degrade to raw `[[div …]]` / `[[/div]]` text (pre-wrap
spans) and memo/table structure collapses. Structural fix (re-pairing
after assembly) was implemented and reverted: revisit with full table
assembly (`[[cell]]`/`[[row]]`/`[[table]]`), not divs alone.

Pages affected (rpc-authority corpus, render-diff verified):

- `nav:side-right` — include ends with two unclosed `[[div_]]`s
  (`desktop-only-container`, `desktop-only`); includer closes at page end:
  - `rpc-archive`
  - `directorate`
  - `lexicon-auctoritas`
- `component:footnote-box` → `footnote-box-base` opens
  `[[div class="creditdrop"]]` + `[[collapsible]]`, never closes them;
  includer ends with `[[/collapsible]] [[/div]]`:
  - `a-summary-of-the-late-history-of-the-auctoritas-imperata`
- `component:mi13adden` → `mi13adden-base` ends mid-table with an
  unclosed `[[cell style="padding: 3px 4px 3px 20px"]]`;
  `component:mi13adden-end` supplies `[[/cell]] [[/row]] [[/table]]`:
  - `celtic-otherworlds`
  - `mi13-format-guide`
  - `mi13-hub`
  - `monarch-security-personnel`
  - `no-0100`
  - `no-0288`
  - `no-0773`
  - `no-1567`
  - `no-1571`
  - `the-history-of-monarch-security`

Obscurative corpus: unaffected.

Separately pre-existing (unrelated to the parser): ListPages item order is
nondeterministic between renders — `_ddtest`, `tales-by-author`,
`mi13-hub`, `index.html` listings etc. flap on re-render.
