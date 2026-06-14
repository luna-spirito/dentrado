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
