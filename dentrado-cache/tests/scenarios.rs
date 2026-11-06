//! Сценарии — перенос прогонов `StorageModel/AtomicScenarios.lean`:
//! замена значения на живом сторе (`replFlow`), трёхактка «коммит или
//! откат» под инъекцией краша (`durK`), ствол-планировщик — два работника
//! × flush (`concurOK`), батон-очередь flush'ов.

use std::rc::Rc;

use dentrado_cache::{Fault, FsDisk, IsFilesystem, MemDisk, Storage, StoreError, TableSpec};

fn run<F: Future>(f: F) -> F::Output {
    compio::runtime::Runtime::new().unwrap().block_on(f)
}

const KEY7: &[u8] = &[7];

/// The three-act values: a full kilobyte each — a leaf-sized inline
/// record every act.
const V1: &[u8] = &[1u8; 1000];
const V2: &[u8] = &[2u8; 1000];

fn specs(ids: &[&[u8]]) -> Vec<TableSpec> {
    ids.iter().map(|i| TableSpec::flat(i.to_vec())).collect()
}

/// Публикация значения: инлайн-запись, никаких страниц — своя
/// однострочная сессия.
fn put_v<D: IsFilesystem>(
    st: &Storage<D>,
    table: &[u8],
    key: &[u8],
    val: &[u8],
) -> Result<(), StoreError> {
    let mut s = st.session();
    s.put(table, key, val, &[], &[])?;
    s.commit()
}

// ## Замена значения на живом сторе

#[test]
fn repl_flow() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        // публикация v1 и коммит
        put_v(&st, b"t0", KEY7, V1).unwrap();
        st.flush().await.unwrap();
        assert_eq!(
            st.get(b"t0", KEY7).await.unwrap(),
            Some(V1.to_vec()),
            "miss-after-commit"
        );
        // замена на v2 — старое дерево умирает прививкой, без слова звонящего
        put_v(&st, b"t0", KEY7, V2).unwrap();
        // дельта прежде дерева: свежая замена видна до flush'а
        assert_eq!(st.get(b"t0", KEY7).await.unwrap(), Some(V2.to_vec()));
        st.flush().await.unwrap();
        let dbg = st.debug_state();
        assert_eq!(dbg.delta_len, 0, "дельта вычищена: {dbg:?}");
        // круг перестроил дерево: старые страницы — в куче смертей (EBR),
        // следующим кругом уедут в свободные клетки
        assert!(dbg.dying_len >= 1, "старое дерево осаждено: {dbg:?}");
        st.partition_exact()
            .await
            .expect("мёртвая страница — клетка цепи");
        assert!(st.ok_store().await, "инвариант против байтов файла");
        // восстановление: новое значение пережило перезапуск
        drop(st);
        let st2 = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        assert_eq!(st2.get(b"t0", KEY7).await.unwrap(), Some(V2.to_vec()));
    });
}

// ## Трёхактка: коммит или откат

async fn act1(st: &Storage<MemDisk>) -> Result<(), StoreError> {
    put_v(st, b"t0", KEY7, V1)?;
    st.flush().await
}

async fn act2(st: &Storage<MemDisk>) -> Result<(), StoreError> {
    put_v(st, b"t0", KEY7, V2)?;
    st.flush().await
}

async fn act3(disk: &MemDisk) -> Option<u8> {
    let st = Storage::open(disk, "db", 4, &specs(&[b"t0"]))
        .await
        .unwrap();
    let v = st.get(b"t0", KEY7).await.unwrap()?;
    // восстановленный образ обязан быть биективным и против байтов файла
    st.partition_exact().await.expect("биекция после краша");
    assert!(st.ok_store().await, "инвариант после краша");
    Some(v[0])
}

/// Трёхактка: акт 1 (спокойствие) — публикация v1 и чекпойнт; акт 2 —
/// замена на v2 под липким сбоем с тика `fault` (≈ краш процесса);
/// разрыв незакоммиченных по предикату; акт 3 — перезапуск и чтение.
async fn three_act(
    fault: u64,
    torn: impl Fn(dentrado_cache::PageId) -> bool + Send + Sync + 'static,
) -> Option<u8> {
    let disk = MemDisk::default();
    let st = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
        .await
        .unwrap();
    act1(&st).await.unwrap();
    drop(st);
    disk.file("db").set_fault(Fault::At(fault));
    if let Ok(st) = Storage::open(&disk, "db", 4, &specs(&[b"t0"])).await {
        let _ = act2(&st).await; // Err ≈ краш рабочей фазы
    }
    disk.file("db").torn_by(torn);
    act3(&disk).await
}

/// Durability замены: карта исходов по тику краша × потоки разрыва —
/// строго бинарная («1» — откат, «2» — коммит), оба исхода существуют, и
/// точка коммона не плавает: в каждом семействе разрывов коммиты —
/// суффикс по тику (позже краш — не раньше отката).
#[test]
fn three_act_outcomes_binary() {
    run(async {
        let mut commits = 0u32;
        let mut rollbacks = 0u32;
        for j in 0..8u64 {
            let torn = move |i: dentrado_cache::PageId| (i.wrapping_mul(j + 1) % 5) == 1;
            let mut first_commit: Option<u64> = None;
            for n in 0..80u64 {
                let out = three_act(n, torn).await;
                assert!(out == Some(1) || out == Some(2), "n={n} j={j}: {out:?}");
                match out {
                    Some(1) => {
                        rollbacks += 1;
                        assert!(
                            first_commit.is_none_or(|c| n < c),
                            "n={n} j={j}: откат после коммита на {first_commit:?}"
                        );
                    }
                    _ => {
                        commits += 1;
                        first_commit.get_or_insert(n);
                    }
                }
            }
        }
        assert!(
            commits > 0 && rollbacks > 0,
            "commits={commits} rollbacks={rollbacks}"
        );
    });
}

/// Спокойная рабочая фаза: перезапуск видит v2 (коммит).
#[test]
fn three_act_quiet_commit() {
    assert_eq!(run(three_act(u64::MAX, |_| false)), Some(2));
}

// ## Ствол-планировщик: работники × flush

/// Работник: раунды «запись-замена» своего ключа; килобайт значения —
/// каждый раунд свежая инлайн-запись.
async fn worker(st: Rc<Storage<MemDisk>>, kb: u8, rounds: u8) -> Result<(), StoreError> {
    let key = [kb];
    for r in 1..=rounds {
        let mut val = vec![0u8; 1000];
        val[0] = kb;
        val[1] = r;
        put_v(&st, b"t0", &key, &val)?;
    }
    Ok(())
}

/// Два работника (разные ключи) и конкурентный flush; после дренажа —
/// финальный flush, инвариант против байтов файла и восстановление
/// последних значений обоих ключей.
#[test]
fn concurrent_workers() {
    run(async {
        let disk = MemDisk::default();
        let st = Rc::new(
            Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
                .await
                .unwrap(),
        );
        let mut ts = Vec::new();
        for kb in [5u8, 9u8] {
            let s = st.clone();
            ts.push(compio::runtime::spawn(worker(s, kb, 6)));
        }
        let s = st.clone();
        ts.push(compio::runtime::spawn(async move { s.flush().await }));
        for t in ts {
            // конкурентный flush может честно отказать — как в модели,
            // его результат не проверяется, финальный обязан
            let _ = t.await.unwrap();
        }
        st.flush().await.unwrap();
        assert!(st.ok_store().await, "инвариант после дренажа");
        // восстановление: оба ключа, последние раунды
        drop(st);
        let st2 = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        for kb in [5u8, 9u8] {
            let v = st2.get(b"t0", &[kb]).await.unwrap().expect("ключ потерян");
            assert_eq!((v[0], v[1]), (kb, 6));
        }
    });
}

// ## Очередь flush'ов: круги агента

#[test]
fn flush_queue() {
    run(async {
        let disk = MemDisk::default();
        let st = Rc::new(
            Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
                .await
                .unwrap(),
        );
        put_v(&st, b"t0", KEY7, V1).unwrap();
        let ts: Vec<_> = (0..4)
            .map(|_| {
                let s = st.clone();
                compio::runtime::spawn(async move { s.flush().await })
            })
            .collect();
        for t in ts {
            t.await.unwrap().unwrap();
        }
        assert!(!st.debug_state().flush_busy, "очередь рассосалась");
        assert!(st.ok_store().await);
    });
}

// ## Мелочи

/// Именованный набор таблиц: записи расходятся по своим таблицам
/// (ключи не пересекаются между таблицами), пустая таблица переживает
/// flush нулевым корнем, открытие с урезанным набором — union: чужие
/// таблицы образа живут дальше (enumerable), дроп таблицы — смерть её
/// страниц и имени, tombstone не переживает процесс.
#[test]
fn table_set_roundtrip() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, &specs(&[b"a", b"b", b"c"]))
            .await
            .unwrap();
        put_v(&st, b"a", b"a", V1).unwrap();
        put_v(&st, b"c", b"a", &[2; 3]).unwrap();
        st.flush().await.unwrap();
        st.partition_exact().await.expect("биекция трёх таблиц");
        drop(st);
        // урезанный набор: образ несёт три таблицы, opener — одну
        let st2 = Storage::open(&disk, "db", 4, &specs(&[b"a"]))
            .await
            .unwrap();
        assert_eq!(st2.tables(), [b"a", b"b", b"c"].map(|s| s.to_vec()));
        assert_eq!(
            st2.get(b"a", b"a").await.unwrap(),
            Some(V1.to_vec()),
            "ключ таблицы a"
        );
        assert!(
            st2.get(b"b", b"a").await.unwrap().is_none(),
            "средняя таблица пуста"
        );
        assert_eq!(st2.get(b"c", b"a").await.unwrap(), Some(vec![2; 3]));
        // дроп: имя уходит, страницы умирают, биекция целых таблиц цела
        st2.drop_table(b"c").await.unwrap();
        assert_eq!(st2.tables(), [b"a", b"b"].map(|s| s.to_vec()));
        assert!(matches!(
            st2.get(b"c", b"a").await,
            Err(StoreError::NoTable)
        ));
        st2.flush().await.unwrap();
        st2.partition_exact().await.expect("биекция после дропа");
        drop(st2);
        // tombstone умирает с процессом: новый образ несёт только живых
        let st3 = Storage::open(&disk, "db", 4, &specs(&[b"a", b"b", b"c"]))
            .await
            .unwrap();
        assert_eq!(st3.tables(), [b"a", b"b", b"c"].map(|s| s.to_vec()));
        assert!(st3.get(b"c", b"a").await.unwrap().is_none());
        assert!(matches!(
            st3.get(b"z", b"a").await,
            Err(StoreError::NoTable)
        ));
    });
}

#[test]
fn fresh_flush_and_reopen() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        st.flush().await.unwrap(); // пустой круг: SB поколения 1
        assert!(st.ok_store().await);
        drop(st);
        let st2 = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        assert_eq!(st2.debug_state().r#gen, 1);
        assert!(st2.get(b"t0", KEY7).await.unwrap().is_none());
    });
}

#[test]
fn dealloc_returns_page() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        let mut s = st.session();
        let k = s.alloc().await.unwrap();
        s.write(k, |p| p.0[0] = 1).await.unwrap();
        s.discard(k).unwrap();
        assert_eq!(s.discard(k), Err(StoreError::UnknownDraft));
        let _k2 = s.alloc().await.unwrap(); // страница вернулась в голову
        drop(s);
        assert!(st.ok_store().await);
    });
}

// ## Настоящий файл (compio::fs)

#[test]
fn fs_disk_smoke() {
    run(async {
        let path =
            std::env::temp_dir().join(format!("dentrado-cache-smoke-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let st = Storage::<FsDisk>::open(&FsDisk, path.to_str().unwrap(), 4, &specs(&[b"t0"]))
                .await
                .unwrap();
            put_v(&st, b"t0", KEY7, V1).unwrap();
            st.flush().await.unwrap();
            assert!(st.ok_store().await);
            assert_eq!(st.get(b"t0", KEY7).await.unwrap(), Some(V1.to_vec()));
        }
        let st = Storage::<FsDisk>::open(&FsDisk, path.to_str().unwrap(), 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        assert_eq!(
            st.get(b"t0", KEY7).await.unwrap(),
            Some(V1.to_vec()),
            "ключ потерян после рестарта"
        );
        assert!(st.ok_store().await);
        drop(st);
        std::fs::remove_file(&path).unwrap();
    });
}
