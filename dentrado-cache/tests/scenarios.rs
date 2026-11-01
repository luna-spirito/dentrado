//! Сценарии — перенос прогонов `StorageModel/AtomicScenarios.lean`:
//! замена значения на живом сторе (`replFlow`), трёхактка «коммит или
//! откат» под инъекцией краша (`durK`), ствол-планировщик — два работника
//! × flush (`concurOK`), батон-очередь flush'ов.

use std::rc::Rc;

use dentrado_cache::core::val::val_split;
use dentrado_cache::page::PageId;
use dentrado_cache::{DraftId, Fault, FsDisk, IsFilesystem, MemDisk, Storage, StoreError};

fn run<F: Future>(f: F) -> F::Output {
    compio::runtime::Runtime::new().unwrap().block_on(f)
}

const KEY7: &[u8] = &[7];

/// Публикация значения из трёх байт: драўт, запись, печать со смертью
/// старых страниц.
async fn put_v<D: IsFilesystem>(
    st: &Storage<D>,
    key: &[u8],
    val: u8,
    freed: &[PageId],
) -> Result<(), StoreError> {
    let k = st.alloc_draft_page().await?;
    st.write_draft_page(k, move |_| (vec![val; 3], ())).await?;
    st.put_cache(0, key, &[0], &[k], freed).await
}

// ## Замена значения на живом сторе

#[test]
fn repl_flow() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, 1).await.unwrap();
        // публикация v1 и коммит
        put_v(&st, KEY7, 1, &[]).await.unwrap();
        st.flush().await.unwrap();
        let (g, cv) = st
            .get_cache(0, KEY7)
            .await
            .unwrap()
            .expect("miss-after-commit");
        // замена на v2 со смертью страниц v1
        put_v(&st, KEY7, 2, &cv.pages).await.unwrap();
        // дельта прежде дерева: свежая замена видна до flush'а
        let (g2, cv2) = st.get_cache(0, KEY7).await.unwrap().unwrap();
        let mid = st
            .read_page_guarded(&g2, cv2.pages[0], |p| val_split(p).1[0])
            .await
            .unwrap();
        assert_eq!(mid, 2);
        drop(g2);
        drop(g); // гарды долой — смерти открылись
        st.flush().await.unwrap();
        let dbg = st.debug_state();
        assert_eq!(dbg.delta_len, 0, "дельта вычищена: {dbg:?}");
        // круг перестроил дерево: старая страница — в куче смертей (EBR),
        // следующим кругом уедет в свободные клетки
        assert_eq!(dbg.dying_len, 1, "старое дерево осаждено: {dbg:?}");
        st.partition_exact()
            .await
            .expect("мёртвая страница — клетка цепи");
        assert!(st.ok_store().await, "инвариант против байтов файла");
        // восстановление: новое значение пережило перезапуск
        drop(st);
        let st2 = Storage::open(&disk, "db", 4, 1).await.unwrap();
        let (g3, cv3) = st2.get_cache(0, KEY7).await.unwrap().unwrap();
        let b = st2
            .read_page_guarded(&g3, cv3.pages[0], |p| val_split(p).1[0])
            .await
            .unwrap();
        assert_eq!(b, 2);
    });
}

// ## Трёхактка: коммит или откат

async fn act1(st: &Storage<MemDisk>) -> Result<(), StoreError> {
    put_v(st, KEY7, 1, &[]).await?;
    st.flush().await
}

async fn act2(st: &Storage<MemDisk>) -> Result<(), StoreError> {
    match st.get_cache(0, KEY7).await? {
        None => Ok(()),
        Some((g, cv)) => {
            let pages = cv.pages.clone();
            drop(g);
            put_v(st, KEY7, 2, &pages).await?;
            st.flush().await
        }
    }
}

async fn act3(disk: &MemDisk) -> Option<u8> {
    let st = Storage::open(disk, "db", 4, 1).await.unwrap();
    let (g, cv) = st.get_cache(0, KEY7).await.unwrap()?;
    let b = st
        .read_page_guarded(&g, cv.pages.first().copied().unwrap_or(0), |p| {
            val_split(p).1[0]
        })
        .await
        .unwrap();
    drop(g);
    // восстановленный образ обязан быть биективным и против байтов файла
    st.partition_exact().await.expect("биекция после краша");
    assert!(st.ok_store().await, "инвариант после краша");
    Some(b)
}

/// Трёхактка: акт 1 (спокойствие) — публикация v1 и чекпойнт; акт 2 —
/// замена на v2 под липким сбоем с тика `fault` (≈ краш процесса);
/// разрыв незакоммиченных по предикату; акт 3 — перезапуск и чтение.
async fn three_act(
    fault: u64,
    torn: impl Fn(PageId) -> bool + Send + Sync + 'static,
) -> Option<u8> {
    let disk = MemDisk::default();
    let st = Storage::open(&disk, "db", 4, 1).await.unwrap();
    act1(&st).await.unwrap();
    drop(st);
    disk.file("db").set_fault(Fault::At(fault));
    if let Ok(st) = Storage::open(&disk, "db", 4, 1).await {
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
            let torn = move |i: PageId| (i.wrapping_mul(j + 1) % 5) == 1;
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

/// Работник: раунды «аллок-запись-замена-со-смертью» своего ключа.
async fn worker(st: Rc<Storage<MemDisk>>, kb: u8, rounds: u8) -> Result<(), StoreError> {
    let key = [kb];
    for r in 1..=rounds {
        let k = st.alloc_draft_page().await?;
        st.write_draft_page(k, move |_| (vec![kb, r], ())).await?;
        let prev_ps = match st.get_cache(0, &key).await? {
            Some((g, cv)) => {
                drop(g);
                cv.pages
            }
            None => Vec::new(),
        };
        let ks = [k];
        let mt = [0u8];
        st.put_cache(0, &key, &mt, &ks, &prev_ps).await?;
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
        let st = Rc::new(Storage::open(&disk, "db", 4, 1).await.unwrap());
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
        let st2 = Storage::open(&disk, "db", 4, 1).await.unwrap();
        for kb in [5u8, 9u8] {
            let (g, cv) = st2
                .get_cache(0, &[kb])
                .await
                .unwrap()
                .expect("ключ потерян");
            let pl = st2
                .read_page_guarded(&g, cv.pages[0], |p| val_split(p).1[..2].to_vec())
                .await
                .unwrap();
            drop(g);
            assert_eq!(pl, vec![kb, 6]);
        }
    });
}

// ## Очередь flush'ов: круги агента

#[test]
fn flush_queue() {
    run(async {
        let disk = MemDisk::default();
        let st = Rc::new(Storage::open(&disk, "db", 4, 1).await.unwrap());
        put_v(&st, KEY7, 1, &[]).await.unwrap();
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

/// Фиксированный набор таблиц: записи расходятся по своим таблицам
/// (ключи не пересекаются между таблицами), пустая таблица переживает
/// flush нулевым корнем, открытие с другим числом таблиц — честный
/// отказ.
#[test]
fn table_set_roundtrip() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, 3).await.unwrap();
        put_v(&st, b"a", 1, &[]).await.unwrap(); // таблица 0
        let k = st.alloc_draft_page().await.unwrap();
        st.write_draft_page(k, |_| (vec![2; 3], ())).await.unwrap();
        st.put_cache(2, b"a", &[0], &[k], &[]).await.unwrap();
        st.flush().await.unwrap();
        st.partition_exact().await.expect("биекция трёх таблиц");
        drop(st);
        let st2 = Storage::open(&disk, "db", 4, 3).await.unwrap();
        let (g, cv) = st2
            .get_cache(0, b"a")
            .await
            .unwrap()
            .expect("ключ таблицы 0");
        let b0 = st2
            .read_page_guarded(&g, cv.pages[0], |p| val_split(p).1[0])
            .await
            .unwrap();
        drop(g);
        assert_eq!(b0, 1);
        assert!(
            st2.get_cache(1, b"a").await.unwrap().is_none(),
            "средняя таблица пуста"
        );
        let (g2, cv2) = st2
            .get_cache(2, b"a")
            .await
            .unwrap()
            .expect("ключ таблицы 2");
        let b2 = st2
            .read_page_guarded(&g2, cv2.pages[0], |p| val_split(p).1[0])
            .await
            .unwrap();
        drop(g2);
        assert_eq!(b2, 2);
        drop(st2);
        assert!(matches!(
            Storage::open(&disk, "db", 4, 2).await,
            Err(StoreError::TableMismatch)
        ));
    });
}

#[test]
fn fresh_flush_and_reopen() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, 1).await.unwrap();
        st.flush().await.unwrap(); // пустой круг: SB поколения 1
        assert!(st.ok_store().await);
        drop(st);
        let st2 = Storage::open(&disk, "db", 4, 1).await.unwrap();
        assert_eq!(st2.debug_state().r#gen, 1);
        assert!(st2.get_cache(0, KEY7).await.unwrap().is_none());
    });
}

#[test]
fn dealloc_returns_page() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, 1).await.unwrap();
        let k: DraftId = st.alloc_draft_page().await.unwrap();
        st.write_draft_page(k, |_| (vec![1], ())).await.unwrap();
        st.dealloc_draft_page(k).await.unwrap();
        assert!(matches!(
            st.dealloc_draft_page(k).await,
            Err(StoreError::UnknownDraft)
        ));
        let _k2 = st.alloc_draft_page().await.unwrap(); // страница вернулась в голову
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
            let st = Storage::<FsDisk>::open(&FsDisk, path.to_str().unwrap(), 4, 1)
                .await
                .unwrap();
            put_v(&st, KEY7, 1, &[]).await.unwrap();
            st.flush().await.unwrap();
            assert!(st.ok_store().await);
            let (g, cv) = st.get_cache(0, KEY7).await.unwrap().expect("ключ потерян");
            let pl = st
                .read_page_guarded(&g, cv.pages[0], |p| val_split(p).1[..2].to_vec())
                .await
                .unwrap();
            drop(g);
            assert_eq!(pl, vec![1, 1]);
        }
        let st = Storage::<FsDisk>::open(&FsDisk, path.to_str().unwrap(), 4, 1)
            .await
            .unwrap();
        let (g, cv) = st
            .get_cache(0, KEY7)
            .await
            .unwrap()
            .expect("ключ потерян после рестарта");
        let pl = st
            .read_page_guarded(&g, cv.pages[0], |p| val_split(p).1[..2].to_vec())
            .await
            .unwrap();
        drop(g);
        assert_eq!(pl, vec![1, 1]);
        assert!(st.ok_store().await);
        drop(st);
        std::fs::remove_file(&path).unwrap();
    });
}
