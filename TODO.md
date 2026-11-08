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

# Wikidot-рендерер (kolorinko): отложенное после батча фиксов render-верности
* Вставки `\n\n` вокруг контента блоков: живой Wikidot (Text_Wiki Div/Collapsible/Divalign
  rules) вставляет пустые строки до/после контента блока, «гася» соседние одиночные
  `\n` (они перестают быть `<br>`). Наш билдер эти вставки не моделирует → расхождение
  на единичный `<br>` на стыках `[[div]]`/`[[collapsible]]` с `@@@@`-строками
  (пример: `_000-2` — у нас `<br><br>`, на живом `<p><br /></p>`). Требует
  моделирования вставок в builder.rs — нетривиально, отложено.
* `<img>` внутри абзаца: живой Paragraph-руль оставляет картинку ВНУТРИ `<p>`, если
  абзац начинается с текста (skip-список смотрит только на ПЕРВЫЙ токен блока). Мы
  всегда рвём абзац перед блоком (`<p>текст</p><img>`). Пример: 001contest…, rpc-000.
* Line-start gating у include/module/`[[module css]]` (pairer-гейт verbatim-CSS) и
  у сплайсинга (`live_directives` требует `^`-якорь, как живой `/^\[\[include …\]\]$/ims`;
  хвост сознательно свободный — живой ленивый `.*?` глотает мусор до следующего `]]\n`,
  наш сбалансированный скан даёт тот же модуль-счётчик: rpc-241). `[[code]]`,
  `[[tabview]]`, галерея и прочие якорные конструкции живого Wikidot
  тоже привязаны к началу строки; у нас парсятся mid-line, т.к. 7 тестов проекта
  зависят от этого. Чинить так же, с аудитом сплайсинга.
* Ненайденная цель include: живой заменяет директиву на
  `[[div class="error-block"]]Page to be included X can not be found![[/div]]`
  (Include.php, process()); мы оставляем литерал/плейсхолдер. Мелкая правка в
  `splice_includes`, но нужен корпус-замер, сколько страниц это заденет.
* Слайс аргументов include: живой (`.*?` + `\]\]$`) может глотать мусор после
  сбалансированного конца (лишняя `]`, текст) до конца строки — мы режем по
  балансу и оставляем хвост как текст (rpc-351: живой съедает лишнюю `]`).
  Появится видимая дивергенция — воспроизводить точно по regex-семантике.
* `[[div]]`: неизвестные атрибуты Wikidot молча выбрасывает, мы сыпем их в `style`.
* `@@@`/`@@@@@` (нечётные хвосты): DOM у нас отличается (пустой pre-wrap span),
  видимый вывод совпадает с живым — ок, но если появится сравнение DOM, учесть.
* tests/dbg_parse.rs — черновой стенд (println без assert'ов, читает /tmp/t1.txt);
  либо превести в ассерты, либо удалить.

## kolorinko: рендер-корпус против золотого Wikidot (2026-09, обход 13 сайтов)

Отлито после фиксов (ul/ol/li, button, footnoteblock title, iframe, перенос
image-директивы через строку, quote_closes `;`); остальное — не «мелкая правка»:

* (2026-09-06, bau home:home) Доделано: data-form страницы (`_template` категории с
  `[[form]]` — источник хранится как `key: value`, wiki-поля одной JSON-строкой с `\n`):
  разворачиваются в layout шаблона с подстановкой `%%form_raw{f}%%` (`forms.rs`); абсолютные
  self-сайтовые инклуды `[[include :site:page]]` теперь сплайсятся (раньше — плейсхолдер);
  остальное в живом — юзер-аватарки `[[user X]]` (avatar.php, данных нет).

* ~~`[[html]] … [[/html]]`~~ — сделано (2026-09-06): verbatim-кадр `Node::Html` по образцу
  `[[code]]`; рендер — same-origin `srcdoc`-iframe в обёртке живого (`html#html-block-html`,
  сброс `html-block.css` инлайном), авторский документ внутри дословно; размер подгоняет
  встроенный ресайзер (`body.scrollHeight` → `frameElement`, `ResizeObserver`), маршрут
  `/page/html/<хэш>` не нужен (в статических дампах были бы мёртвые маршруты). Изоляция как в
  живом: блочный CSS (`body`/`:root`/`*`) и скрипты заперты в iframe. Интерьер вербатимен:
  викитекст внутри `[[html]]` виден литералом — так же, как в живом (проверено на it-cea
  cea-063: золотой отдаёт `Sede Andromeda[[footnote]]…` дословно). Подробности — DIVERGENCE.md.
* `@<…>@` (raw-HTML passthrough, bau nav:top: `Home @<}&nbsp;>@`) — рендерится литералом.
  Живой отдаёт содержимое между `@<` и `>@` как есть (несэкейпленным). Мелкий фикс в лексере,
  но это новая конструкция — отдельным заходом.
* `[[embed]]`/`[[embedurl]]`/`[[embedvideo]]` (srddf ~96 стр., rpc site-index):
  живой гоняет Google-Gadget XML (gmodules.com) с параметрами; контент третьесторонний
  по природе. Нужен шаблон гаджета + iframe-рендер, либо осознанный заглушка-плейсхолдер.
* `[[math]]` (cea-017, rpc-133/501, obscurative teleportation-1): живой — LaTeX→картинка
  (images.math.wikidot.com). Самодостаточность = свой рендер формул или пререндер на инжесте.
* `[[gallery]]`/FilesGallery (sci-alliance 020/uniform/clothes, srddf files-gallery):
  нужен индекс файлов страницы из экспорта + модуль-рендер.
* `[[toc]]` (bau level-31/faq, rpc 486-security-files, wci site-charter…): якоря уже
  расставлены (`assign_toc_anchors`), но боксу нужен проход по заголовкам всей страницы —
  рендер-контекст, а не поддерево.
* `[[bibliography]]`/`[[bib]]` (rpc celtic-otherworlds и др.): отдельная маленькая фича.
* Дубли якорей `toc0…`: сайдбар и контент страницы парсятся независимо — оба стартуют
  с `toc0`, id в документе дублируются. Фикс: оффсет/неймспейс якорей шелла.
* Форум-ссылки (`/forum/c-N/…`, forum:start): в экспорте данных форума нет; ссылки
  ведут наружу (bau — на все 861 стр.) или в 404. Нужно решение: рвать/заглушка/оставить.
* Неэкспортированные файлы чужих сайтов: rpcsandbox.wdfiles.com (150+ стр. rpc),
  bau-oversight `N.png` (861 стр.), theta-default.png, sandbox-картинки — лежат в файловых
  хранилищах ДРУГИХ сайтов Wikidot. Самодостаточность = политика скачивания хотлинков.
* Внешние CDN: fonts.googleapis.com (bau, 151 стр.), use.fontawesome.com/code.jquery.com
  (irregularity, sci-alliance темы) — третьесторонние `<link>` на каждой странице.
* `{$var}` без значения в источнике `[[image {$logo} …]]` (wci component:member-list-mini):
  живой оставляет `{$logo}` литералом в src; у нас пустая подстановка валит директиву в raw.
  Нужен фолбэк «неразвязанная include-переменная → литерал» в источниках картинок.
* `og:description` содержит сырой викитекст (деградировавшие директивы попадают в
  первый параграф) — фильтр Raw-узлов при сборе описания.
* Косметика (невидимо): tachys SSR ставит `<!>`-маркеры гидрации (~1 на конец абзаца,
  браузер игнорирует); `class=""` печатается и без класса у div/span/li/… (CSS-эквивалентно).

  ```
  

Nov 01 05:00:28 dentrado kolorinko[238178]: [2026-11-01T05:00:28Z INFO  kolorinko::metrics] subs_active=0 gear_running=0 sub_first n=0 p50=- p99=- max=-
Nov 01 05:01:28 dentrado kolorinko[238178]: [2026-11-01T05:01:28Z INFO  kolorinko::metrics] subs_active=0 gear_running=0 sub_first n=0 p50=- p99=- max=-
Nov 01 05:02:28 dentrado kolorinko[238178]: [2026-11-01T05:02:28Z INFO  kolorinko::metrics] subs_active=0 gear_running=0 sub_first n=0 p50=- p99=- max=-
Nov 01 05:03:28 dentrado kolorinko[238178]: [2026-11-01T05:03:28Z INFO  kolorinko::metrics] subs_active=0 gear_running=1 sub_first n=0 p50=- p99=- max=-
Nov 01 05:04:28 dentrado kolorinko[238178]: [2026-11-01T05:04:28Z INFO  kolorinko::metrics] subs_active=0 gear_running=0 sub_first n=0 p50=- p99=- max=-
Nov 01 05:05:28 dentrado kolorinko[238178]: [2026-11-01T05:05:28Z INFO  kolorinko::metrics] subs_active=0 gear_running=0 sub_first n=0 p50=- p99=- max=-
Nov 01 05:06:28 dentrado kolorinko[238178]: [2026-11-01T05:06:28Z INFO  kolorinko::metrics] subs_active=0 gear_running=0 sub_first n=0 p50=- p99=- max=-
Nov 01 05:07:28 dentrado kolorinko[238178]: [2026-11-01T05:07:28Z INFO  kolorinko::metrics] subs_active=0 gear_running=0 sub_first n=0 p50=- p99=- max=-
Nov 01 05:07:39 dentrado kolorinko[238178]: thread 'dentrado-core-1' (238266) panicked at apps/kolorinko/src/wikidot_parser/pairer.rs:273:18:
Nov 01 05:07:39 dentrado kolorinko[238178]: quote depth without a frame
Nov 01 05:07:39 dentrado kolorinko[238178]: note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
Nov 01 05:07:39 dentrado kolorinko[238178]: [2026-11-01T05:07:39Z ERROR dentrado::core] core 1: task panicked — stopping the node's cores
Nov 01 05:07:39 dentrado kolorinko[238178]: dentrado: warning: a Shared handle outlived its Core — leaking its allocation (i'm not dealing with it rn)
Nov 01 05:07:39 dentrado kolorinko[238178]: dentrado: warning: a Shared handle outlived its Core — leaking its allocation (i'm not dealing with it rn)
Nov 01 05:07:39 dentrado kolorinko[238178]: dentrado: warning: a Shared handle outlived its Core — leaking its allocation (i'm not dealing with it rn)
Nov 01 05:07:39 dentrado kolorinko[238178]: dentrado: warning: a Shared handle outlived its Core — leaking its allocation (i'm not dealing with it rn)
Nov 01 05:07:39 dentrado kolorinko[238178]: dentrado: warning: a Shared handle outlived its Core — leaking its allocation (i'm not dealing with it rn)
Nov 01 05:07:39 dentrado kolorinko[238178]: dentrado: warning: a Shared handle outlived its Core — leaking its allocation (i'm not dealing with it rn)
Nov 01 05:07:39 dentrado kolorinko[238178]: dentrado: warning: a Shared handle outlived its Core — leaking its allocation (i'm not dealing with it rn)
Nov 01 05:07:39 dentrado kolorinko[238178]: dentrado: warning: a Shared handle outlived its Core — leaking its allocation (i'm not dealing with it rn)
Nov 01 05:07:39 dentrado kolorinko[238178]: dentrado: warning: a Shared handle outlived its Core — leaking its allocation (i'm not dealing with it rn)
Nov 01 05:07:39 dentrado kolorinko[238178]: dentrado: warning: a Shared handle outlived its Core — leaking its allocation (i'm not dealing with it rn)
Nov 01 05:07:39 dentrado kolorinko[238178]: dentrado: warning: a Shared handle outlived its Core — leaking its allocation (i'm not dealing with it rn)
Nov 01 05:07:39 dentrado kolorinko[238178]: dentrado: warning: a Shared handle outlived its Core — leaking its allocation (i'm not dealing with it rn)
Nov 01 05:07:39 dentrado kolorinko[238178]: dentrado: warning: a Shared handle outlived its Core — leaking its allocation (i'm not dealing with it rn)
  
  ```

----

Нашёл запись в `files.json`: файл числится со `status: "saved"` и блобом `files_ca/dd/1a/2ced...`. Проверю, существует ли блоб на диске и что отдаёт HTTP.

Блоб на диске есть (620 KB, сохранён сегодня в 10:37), но HTTP отдаёт 404 на обоих хостах. Копаю дальше: тело 404, свежесть индекса, работает ли другой файл.

Паблишер — отдельный бинарник `kolorinko` со своим конфигом, запущен сегодня 16:20. Смотрю его конфиг и сервис, проверяю страницы vs файлы.

Конфиг маппинг имеет: `backroomsru.dentrado.art` → сайт `www.backroomswiki.ru`. Но файлы не отдаются. Смотрю логи kolorinko и характер 404 (страницы/файлы/другие сайты).

Страницы работают (200), а файлы 404 на всех сайтах — похоже, kolorinko вообще не отдаёт `local--files`. Проверю пути в files.json викидот-сайтов и найду исходники kolorinko локально.

kolorinko не читает `files.json` напрямую — видимо, импортирует out/ в свою БД. Ищу ingestion-код.

Серверный код kolorinko живёт в `/home/luna/prj/dentrado/dentrado/`. Ищу там логику отдачи файлов.

Это проекции датасета. Теперь посмотрю, как HTTP-слой обрабатывает запрос `/local--files/...` и как out_walk импортирует files.json.

CA-URL формируется как `/-/repo/<site>/files/<xx>/<yy>/<hash>.<ext>` — то есть kolorinko раздаёт файлы через системный префикс, а не по оригинальным путям. Проверю, работает ли CA-URL баннера на сервере и есть ли в HTML страницы перезаписанная ссылка.

CA-URL отдаёт 302, а в HTML ссылки остались оригинальными (`/local--files/...`). Смотрю, куда ведёт редирект и как out_walk импортирует файлы.

Нашёл кое-что странное: CA-URL отвечает 302 с **перепутанным** location: `//1a/<hash>.png/files/dd/repo/www.backroomswiki.ru` — сегменты пути перевёрнуты. Смотрю `repo::serve`.

`repo::serve` тут вообще не может дать 302 — только 404 или 200. Значит, что-то ещё перехватывает. Но важнее главного: в отрендеренной HTML ссылки остались сырыми `/local--files/...` — подстановка на CA-URL не сработала. Смотрю пайплайн перезаписи ссылок.

Пазл складывается: в HTML ссылки относительные (`/local--files/...`), а резолвер собирает только абсолютные `host/path`-хвосты. Проверю `http_tail` и `canon_file_key`.

`http_tail(u, None)` для относительного URL возвращает `None` — значит относительные ссылки вообще не собираются резолвером. Но ещё странно 302 от CA-URL. Проверю `SYSTEM_PREFIX` и свежесть задеплоенного бинарника.

В локальном репо dentrado есть свежайший коммит сегодня: `e2e3dd5 fix(k): hydration crash + banner link` (19:18 +0300), а сервис перезапущен в 19:20 +0300. Смотрю, что он чинит и собран ли деплой из него.

Этот коммит про баннер-ссылку на исходную вики, не про вложения. Надо перепроверить мой ранний тест CA-URL — я делал curl без `--resolve`, и DNS мог увести запрос на реальный сайт, а не на зеркало.

Всё встало на место: CA-URL через зеркало отдаёт **200 image/png** — файл на месте. А мой ранний 302 был с реального сайта (www.backroomswiki.ru резолвится в 176.108.249.243, не в зеркало) — ложный след. Теперь подтвержу, что ссылки в HTML не перезаписаны, и сравню с викидот-сайтом.

Подтверждено: весь HTML зеркала rufoundation-сайта усыпан неперезаписанными относительными `/local--files/...`. Быстро гляну, как evakuilo сохраняет тела страниц rufoundation — чтобы понять, с какой стороны чинить.

Баннера нет в pages.json — значит, он в шелле (шапке сайта). Проверяю.

Странно — в pages.json и shell баннера нет, но SSR его отдаёт. Смотрю, где живёт тело главной страницы (pages_by_id).

Нигде в публикации literal-ссылки на баннер нет, кроме files.json — но SSR HTML её выдал. Проверяю сырой HTML страницы ещё раз, с контекстом.

Полная ясность. В состоянии страницы видно: `[[image Backrooms_Banner_Rus.png link="/local--files/main/Backrooms_Banner_Rus.png"]]` — источник голого имени файла и относительной ссылки. Гляну, как рендерер обрабатывает голый `[[image name]]`.

Диагностика завершена — причинно-следственная цепочка полностью восстановлена. Вот итог:

## Файл не потерян — он есть и отдаётся

- evakuilo сохранил баннер: в `files.json` сайта `www.backroomswiki.ru` запись со `status: "saved"`, sha256 `dd1a2c…`, 620 712 байт; блоб лежит на диске (`files_ca/dd/1a/2ced…`).
- Через CA-URL зеркало отдаёт его нормально — проверено с сервера:
  ```
  https://backroomsru.dentrado.art/-/repo/www.backroomswiki.ru/files/dd/1a/dd1a2ced….png → 200, image/png
  ```

## Почему оригинальный путь даёт 404

**kolorinko вообще не раздаёт `/local--files/…`** — это by design ([repo.rs:16-19](/home/luna/prj/dentrado/dentrado/apps/kolorinko/src/repo.rs#L16-L19): «there is no path-based form to serve here»). Вложения живут только в системном неймспейсе `/-/repo/<site>/files/<hash>`, а ссылки в страницах должны перезаписываться на CA-URL при рендере.

## Почему ссылка на баннер не перезаписалась

На странице баннер вставлен как `[[image Backrooms_Banner_Rus.png link="/local--files/main/Backrooms_Banner_Rus.png"]]` — **обе формы относительные**. А пасс ресурсов собирает только абсолютные `http(s)://`-хвосты:

- [resources.rs:94](/home/luna/prj/dentrado/dentrado/apps/kolorinko/src/wikidot_page/resources.rs#L94) — `http_tail(&url, None)` с `base=None`;
- [css.rs:180](/home/luna/prj/dentrado/dentrado/apps/kolorinko-render/src/css.rs#L180) — для URL без схемы и без `//` ответ `let base = base?` → `None` → ссылка не собирается → не резолвится;
- [render.rs:964](/home/luna/prj/dentrado/dentrado/apps/kolorinko-render/src/render.rs#L964) — `render_image` пишет `src` как есть.

В итоге HTML зеркала содержит сырые `href="/local--files/main/…"` и `src="Backrooms_Banner_Rus.png"`, браузер просит это у зеркала → 404. Битыми на зеркале будут **все** относительные вложения (иконки навигации `noaccess:nav-pages-css` и т.д.) — я это подтвердил на выдаче.

## Замечание

Мой первый тест CA-URL дал ложный 302 — curl без `--resolve` уходил на реальный сайт: `www.backroomswiki.ru` резолвится в `176.108.249.243`, а не в зеркало. Зеркало — только `backroomsru.dentrado.art`.

## Где чинить

Два варианта:
1. **kolorinko** (предпочтительнее): в пассе ресурсов абсолютизировать относительные ссылки против хоста сайта (`http_tail(u, Some(base))`), плюс развернуть голую wikidot-форму `[[image name]]` → `local--files/{slug}/{name}`. Чинит уже собранный корпус без пере-краулинга.
2. **evakuilo**: абсолютизировать URL при сохранении страниц — но потребует пере-краула rufoundation-сайтов.

Сказать, чтобы я взялся за фикс в kolorinko (вариант 1)?
```
  
* I believe something's fishy with `repo` gear, and it's probably being awaited on the hot path?
Whatever, it needs caching and proper background update smeanics

----
- Proper lens
- Rererere-review contact APIs...
- zeropool...
- sure blake3 for gear keys?
- multiple tables for different gear cache kinds.

- Currently gears that perform IO can fail, and that poisons the cache since they're total. Probably need to mark gears as poisoned, idk... And make a new pass to raise alarms.
