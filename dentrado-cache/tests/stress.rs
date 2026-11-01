//! Нагрузка поверх сценариев: рукодельный образ со входом-смещением
//! (семантика «off живёт во входе»), многочанковый flush (цепочки
//! ≥2 chunk-страниц на пул), случайная нагрузка с теневой моделью и
//! поздними гардами, крошечный пул слотов, расхождение смертей с
//! моделью (pending-бакет EBR) и поедание восстановленного остатка
//! (off≠0-вход) кругом flush'а.

use std::collections::HashMap;

use dentrado_cache::core::freelist::{ChainEntry, write_chunk};
use dentrado_cache::core::sb::write_sb;
use dentrado_cache::core::val::val_split;
use dentrado_cache::page::{Page, PageId, fnv_id};
use dentrado_cache::{Guard, IsFilesystem, MemDisk, Storage};

fn run<F: Future>(f: F) -> F::Output {
    compio::runtime::Runtime::new().unwrap().block_on(f)
}

// ## Вход со смещением: off — атрибут входа, не страницы

/// Рукодельный образ мира из 14 страниц: chunk-страница 2 держит слоты
/// [4, 6, 8, 10, 12], вход чётного пула — ⟨2, off 2⟩, root нет. Живы
/// чётные слоты [8, 10, 12]; мёртвые [4, 6] впереди не выдаются и в
/// счётчики не попадают.
async fn offset_image(disk: &MemDisk) {
    let f = <MemDisk as IsFilesystem>::open(disk, "db").await.unwrap();
    let mut ch = Page::zeroed();
    write_chunk(&mut ch, 0, &[4, 6, 8, 10, 12]);
    <MemDisk as IsFilesystem>::write_page(f.clone(), 2, ch.0.to_vec())
        .await
        .0
        .unwrap();
    let z = Page::zeroed().0.to_vec();
    for id in 3..14 {
        <MemDisk as IsFilesystem>::write_page(f.clone(), id, z.clone())
            .await
            .0
            .unwrap();
    }
    let mut sbp = Page::zeroed();
    // мир из 14 страниц: чётный счётчик 14 (0 — SB, 2 — chunk, слоты
    // 4..12), нечётный 15 (1 — SB, 3..13 — в мире, но не в цепях)
    write_sb(
        &mut sbp,
        1,
        14,
        15,
        &[0],
        ChainEntry { page: 2, off: 2 },
        ChainEntry::default(),
        3,
        0,
        fnv_id(&[8, 10, 12]),
        fnv_id(&[]),
    );
    <MemDisk as IsFilesystem>::write_page(f.clone(), 0, sbp.0.to_vec())
        .await
        .0
        .unwrap();
}

/// Три alloc'а из цепочки и dealloc'и назад: слоты возвращаются в
/// голову своего пула, рост мира не происходит.
#[test]
fn image_entry_offset() {
    run(async {
        let disk = MemDisk::default();
        offset_image(&disk).await;

        let st = Storage::open(&disk, "db", 4, 1).await.unwrap();
        assert_eq!(st.debug_state().free_nexts, [14, 15], "мир читан из SB");
        let mut ks = Vec::new();
        for _ in 0..3 {
            ks.push(st.alloc_draft_page().await.unwrap());
        }
        assert_eq!(
            st.debug_state().free_nexts,
            [14, 15],
            "три страницы — из цепочки, не ростом мира"
        );
        for k in ks {
            st.dealloc_draft_page(k).await.unwrap();
        }
        let mut head = st.debug_state().heads[0].clone();
        head.sort_unstable();
        assert_eq!(head, vec![8, 10, 12], "мёртвые слоты пропущены");
        assert!(st.ok_store().await);
    });
}

// ## Многочанковые цепочки

/// Значение на ~1100 страниц, публикация, затем смерть: смерти уходят
/// в цепочки из ≥2 chunk-страниц (511 слотов); инвариант против байтов
/// проверяет счётчики и суммы обеих цепочек, восстановление раздаёт
/// запас из цепочек, пере-публикация ест их заново.
#[test]
fn multi_chunk_flush() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, 1).await.unwrap();
        let key = b"big";
        let mut ks = Vec::new();
        for _ in 0..1100 {
            let k = st.alloc_draft_page().await.unwrap();
            st.write_draft_page(k, |_| (vec![7u8; 3], ()))
                .await
                .unwrap();
            ks.push(k);
        }
        st.put_cache(0, key, &[0], &ks, &[]).await.unwrap();
        st.flush().await.unwrap();
        assert!(st.ok_store().await);
        let (g, cv) = st.get_cache(0, key).await.unwrap().expect("ключ");
        assert_eq!(cv.pages.len(), 1100);
        let old = cv.pages;
        drop(g);
        // замена на одну страницу: 1100 умирают — в цепи своего пула
        // (свежий свободный запас: клетки пишутся один раз, дальше их
        // только едят — голова пула остаётся пустой)
        let k2 = st.alloc_draft_page().await.unwrap();
        st.write_draft_page(k2, |_| (vec![1, 2, 3], ()))
            .await
            .unwrap();
        st.put_cache(0, key, &[0], &[k2], &old).await.unwrap();
        // гвардов нет — reg сам осаждает свою партию, печать забирает
        // смерти без пин-цикла (случай под гвардом — death_pending_bucket)
        st.flush().await.unwrap();
        assert!(
            st.ok_store().await,
            "счётчики и суммы многочанковых цепочек"
        );
        let dbg = st.debug_state();
        // круг перестроил дерево: старая страница — в куче EBR (до неё
        // дотянется следующий круг), головы пусты — запас значений живёт
        // в цепи чётного пула
        assert_eq!(dbg.dying_len, 1, "{dbg:?}");
        assert!(
            dbg.heads[0].is_empty() && dbg.heads[1].is_empty(),
            "смерти уехали в цепь чётного пула: {dbg:?}"
        );
        st.partition_exact().await.expect("смерти — клетки цепей");
        drop(st);
        // восстановление: запас живёт в цепочках, головы пусты
        let st2 = Storage::open(&disk, "db", 4, 1).await.unwrap();
        let (g, cv2) = st2.get_cache(0, key).await.unwrap().expect("ключ");
        assert_eq!(cv2.pages.len(), 1);
        drop(g);
        let dbg2 = st2.debug_state();
        assert!(
            dbg2.heads[0].is_empty() && dbg2.heads[1].is_empty(),
            "рестарт видит запас в хвостах: {dbg2:?}"
        );
        let _k = st2.alloc_draft_page().await.unwrap();
        let dbg3 = st2.debug_state();
        assert!(
            dbg3.heads[0].len() + dbg3.heads[1].len() >= 500,
            "чанк целиком в голову: {dbg3:?}"
        );
        assert!(st2.ok_store().await);
        // пере-публикация поверх цепочек: flush ест чужой пул целиком
        let mut ks2 = Vec::new();
        for _ in 0..5 {
            let k = st2.alloc_draft_page().await.unwrap();
            st2.write_draft_page(k, |_| (vec![9; 3], ())).await.unwrap();
            ks2.push(k);
        }
        st2.put_cache(0, b"two", &[0], &ks2, &[]).await.unwrap();
        st2.flush().await.unwrap();
        assert!(st2.ok_store().await, "выедание цепочки с дренажом");
    });
}

// ## Случайная нагрузка с теневой моделью

/// 8 ключей, 260 операций: публикации 0..2500 байт, чтения со сверкой,
/// удаления, flush'ы; гарды иногда переживают flush — смерти догоняются
/// позже. Финал — flush, сверка, перезапуск, полная сверка, инвариант.
#[test]
fn random_ops_smoke() {
    run(async {
        let mut seed = 0x853c49e6748fea9bu64;
        let mut rnd = move || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            seed
        };
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, 1).await.unwrap();
        let mut shadow: HashMap<u8, (u8, Vec<u8>)> = HashMap::new();
        let mut held: Vec<Guard<MemDisk>> = Vec::new();
        for op in 0..260u32 {
            let r = rnd();
            let key = (r % 8) as u8;
            match r % 20 {
                0..=11 => {
                    let ver = (op % 251) as u8;
                    let len = ((r >> 32) % 2500) as usize;
                    let payload: Vec<u8> = (0..len as u32)
                        .map(|i| key.wrapping_add(ver).wrapping_add(i as u8))
                        .collect();
                    // страница значения не хранит длину: payload добивается
                    // нулями до 4080, длину возит mt
                    let mut mt = vec![ver];
                    mt.extend_from_slice(&(len as u32).to_le_bytes());
                    let mut ks = Vec::new();
                    for c in payload.chunks(4080) {
                        let k = st.alloc_draft_page().await.unwrap();
                        let c = c.to_vec();
                        st.write_draft_page(k, move |_| (c.clone(), ()))
                            .await
                            .unwrap();
                        ks.push(k);
                    }
                    let prev = match st.get_cache(0, &[key]).await.unwrap() {
                        Some((g, cv)) => {
                            drop(g);
                            cv.pages
                        }
                        None => Vec::new(),
                    };
                    st.put_cache(0, &[key], &mt, &ks, &prev).await.unwrap();
                    shadow.insert(key, (ver, payload));
                }
                12..=16 => {
                    if let Some((g, cv)) = st.get_cache(0, &[key]).await.unwrap() {
                        let mut got = Vec::new();
                        for &p in &cv.pages {
                            let b = st
                                .read_page_guarded(&g, p, |pg| val_split(pg).1.to_vec())
                                .await
                                .unwrap();
                            got.extend(b);
                        }
                        let len = cv
                            .mt
                            .get(1..5)
                            .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)
                            .unwrap_or(0);
                        got.truncate(len);
                        let (_, want) = shadow.get(&key).cloned().unwrap_or((0, vec![]));
                        assert_eq!(got, want, "op {op}, ключ {key}");
                        if op % 7 == 0 {
                            held.push(g); // гард переживёт flush
                        } else {
                            drop(g);
                        }
                    }
                }
                17..=18 => {
                    let prev = match st.get_cache(0, &[key]).await.unwrap() {
                        Some((g, cv)) => {
                            drop(g);
                            cv.pages
                        }
                        None => continue,
                    };
                    st.put_cache(0, &[key], &[], &[], &prev).await.unwrap();
                    shadow.insert(key, (0, vec![]));
                }
                _ => {
                    st.flush().await.unwrap();
                }
            }
            if op % 40 == 39 {
                held.clear();
            }
        }
        held.clear();
        st.flush().await.unwrap();
        let dbg = st.debug_state();
        // финальный круг перестроил таблицу: старое дерево (одна
        // страница — ключей мало) — в куче EBR
        assert_eq!(dbg.dying_len, 1, "старое дерево финального круга: {dbg:?}");
        assert!(st.ok_store().await);
        st.partition_exact().await.expect("биекция финала");
        drop(st);

        let st2 = Storage::open(&disk, "db", 4, 1).await.unwrap();
        for key in 0..8u8 {
            match st2.get_cache(0, &[key]).await.unwrap() {
                Some((g, cv)) => {
                    let mut got = Vec::new();
                    for &p in &cv.pages {
                        let b = st2
                            .read_page_guarded(&g, p, |pg| val_split(pg).1.to_vec())
                            .await
                            .unwrap();
                        got.extend(b);
                    }
                    drop(g);
                    let len = cv
                        .mt
                        .get(1..5)
                        .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)
                        .unwrap_or(0);
                    got.truncate(len);
                    let (_, want) = shadow.get(&key).cloned().unwrap_or((0, vec![]));
                    assert_eq!(got, want, "ключ {key} после рестарта");
                }
                None => assert!(
                    !shadow.contains_key(&key),
                    "ключ {key} потерян после рестарта"
                ),
            }
        }
        assert!(st2.ok_store().await);
        st2.partition_exact().await.expect("биекция после рестарта");
    });
}

// ## Крошечный пул

/// Два слота арены: вытеснение на каждом шаге — драўты, чтения и
/// сохранения чередуются на грани емкости.
#[test]
fn tiny_pool_stress() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 2, 1).await.unwrap();
        for r in 0..40u8 {
            let k = st.alloc_draft_page().await.unwrap();
            st.write_draft_page(k, move |_| (vec![r; 100], ()))
                .await
                .unwrap();
            st.put_cache(0, &[r], &[r], &[k], &[]).await.unwrap();
        }
        st.flush().await.unwrap();
        assert!(st.ok_store().await);
        for r in 0..40u8 {
            let (g, cv) = st.get_cache(0, &[r]).await.unwrap().expect("ключ");
            let b = st
                .read_page_guarded(&g, cv.pages[0], |p| val_split(p).1[..3].to_vec())
                .await
                .unwrap();
            drop(g);
            assert_eq!(b, vec![r; 3]);
        }
        drop(st);
        let st2 = Storage::open(&disk, "db", 2, 1).await.unwrap();
        let (g, cv) = st2.get_cache(0, &[7]).await.unwrap().expect("ключ");
        let b = st2
            .read_page_guarded(&g, cv.pages[0], |p| val_split(p).1[..3].to_vec())
            .await
            .unwrap();
        drop(g);
        assert_eq!(b, vec![7; 3]);
        assert!(st2.ok_store().await);
    });
}

// ## Смерти в pending-бакете: расхождение с моделью
//
// Порт: EBR осаждает смерти в pending-бакет текущей эпохи, дренаж —
// только полный пустой оборот трёх бакетов (анпин/рег). Печать
// забирает лишь осевшую кучу dying. Без гвардов reg сам проходит
// оборот и осаждает свою партию в кучу — печать забирает её тем же
// раундом. Расхождение с моделью остаётся для смертей под живым
// гвардом: оборот невозможен, смерти ждут его ухода, раунд пропускают,
// страницы не попадают ни в головы, ни в новый SB. Пока стор жив, они
// догоняются позже — это и показано ниже. Модель же держит плоский
// список смертей, и та же самая печать обязана его забрать. Цена
// расхождения — окно: умри процесс после коммита нового значения, но
// до дренажа, и страницы навсегда выпадают из образа (ни корень, ни
// сток их не знают, мир не сжимается).
#[test]
fn death_pending_bucket() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, 1).await.unwrap();
        let key = b"big";
        let mut ks = Vec::new();
        for i in 0..6u8 {
            let k = st.alloc_draft_page().await.unwrap();
            st.write_draft_page(k, move |_| (vec![i; 3], ()))
                .await
                .unwrap();
            ks.push(k);
        }
        st.put_cache(0, key, &[0], &ks, &[]).await.unwrap();
        st.flush().await.unwrap();
        let (g, cv) = st.get_cache(0, key).await.unwrap().expect("ключ");
        let old = cv.pages;
        let k2 = st.alloc_draft_page().await.unwrap();
        st.write_draft_page(k2, |_| (vec![1, 2, 3], ()))
            .await
            .unwrap();
        // смерть и печать — под живым гвардом: оборот руки невозможен,
        // смерти сидят в pending-бакете и раунд пропускают
        st.put_cache(0, key, &[0], &[k2], &old).await.unwrap();
        st.flush().await.unwrap();
        let dbg = st.debug_state();
        assert_eq!(dbg.dying_len, 0, "{dbg:?}");
        assert!(
            old.iter()
                .all(|p| !dbg.heads[0].contains(p) && !dbg.heads[1].contains(p)),
            "смерти пропустили печать — сидят в pending-бакете: {dbg:?}"
        );
        assert!(st.ok_store().await);
        // уход гварда осаждает кучу: шесть страниц значения и старое
        // дерево круга (оно тоже сидело в pending — гвард защищал
        // читателя посреди спуска)
        drop(g);
        assert_eq!(st.debug_state().dying_len, old.len() + 1, "дренаж в кучу");
        st.flush().await.unwrap();
        let dbg = st.debug_state();
        assert!(
            old.iter()
                .all(|p| dbg.heads[0].contains(p) || dbg.heads[1].contains(p)),
            "смерти догнаны второй печатью: {dbg:?}"
        );
        assert!(st.ok_store().await);
    });
}

// ## Смерти под гвардом через печать: краш не теряет страницы

/// Смерти, ждущие гварда на момент печати, обязаны быть упакованы
/// клетками (лимбо) — краш стирает гварды, и страницы становятся
/// свободными по образу; без упаковки они выпадают из мира навсегда
/// (ни корень, ни цепи их не знают).
#[test]
fn pending_deaths_survive_crash() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, 1).await.unwrap();
        let key = b"k";
        let mut ks = Vec::new();
        for i in 0..4u8 {
            let k = st.alloc_draft_page().await.unwrap();
            st.write_draft_page(k, move |_| (vec![i; 3], ()))
                .await
                .unwrap();
            ks.push(k);
        }
        st.put_cache(0, key, &[0], &ks, &[]).await.unwrap();
        st.flush().await.unwrap();
        let (g, cv) = st.get_cache(0, key).await.unwrap().expect("ключ");
        let old = cv.pages;
        let k2 = st.alloc_draft_page().await.unwrap();
        st.write_draft_page(k2, |_| (vec![9; 3], ())).await.unwrap();
        st.put_cache(0, key, &[0], &[k2], &old).await.unwrap();
        st.flush().await.unwrap(); // смерти — под гвардом: pending
        drop(g);
        drop(st); // краш: гварды и арена исчезли, образ — как закоммичен
        let st2 = Storage::open(&disk, "db", 4, 1).await.unwrap();
        st2.partition_exact()
            .await
            .expect("pending-смерти упакованы клетками");
        assert!(st2.ok_store().await);
    });
}

/// Живые черновики на момент печати: краш стирает арену, их страницы
/// обязаны быть клетками (лимбо) — иначе они навсегда выпадают из
/// мира.
#[test]
fn live_drafts_survive_crash() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, 1).await.unwrap();
        let mut ks = Vec::new();
        for i in 0..6u8 {
            let k = st.alloc_draft_page().await.unwrap();
            st.write_draft_page(k, move |_| (vec![i; 3], ()))
                .await
                .unwrap();
            ks.push(k);
        }
        st.flush().await.unwrap(); // черновики живы на печати
        drop(st); // краш
        let st2 = Storage::open(&disk, "db", 4, 1).await.unwrap();
        st2.partition_exact()
            .await
            .expect("страницы черновиков — клетки");
        assert!(st2.ok_store().await);
    });
}

/// Разбиение страниц в покое: ниже счётчиков образа каждая страница —
/// корневая, клетка цепи или held в RAM (головы, черновики, недретые
/// смерти).
#[test]
fn page_partition() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, 1).await.unwrap();
        let key = b"big";
        let mut old: Vec<PageId> = Vec::new();
        for r in 0..3u8 {
            let mut ks = Vec::new();
            for i in 0..6u8 {
                let k = st.alloc_draft_page().await.unwrap();
                st.write_draft_page(k, move |_| (vec![i + r; 3], ()))
                    .await
                    .unwrap();
                ks.push(k);
            }
            st.put_cache(0, key, &[0], &ks, &old).await.unwrap();
            {
                let (g, cv) = st.get_cache(0, key).await.unwrap().expect("ключ");
                old = cv.pages;
                drop(g);
            }
            st.flush().await.unwrap();
            st.partition_exact()
                .await
                .unwrap_or_else(|v| panic!("раунд {r}: {v:?}"));
        }
        // вычерпать головы аллокациями — принудить поедание хвоста
        for _ in 0..40 {
            st.alloc_draft_page().await.unwrap();
        }
        st.partition_exact().await.expect("биекция после дренажа");
    });
}

/// Контейнер съеденного задачами чанка уходит в `spent`, печать пакует
/// его клеткой в свои чанки; следующий круг суперседит те чанки оптом,
/// а пере-листинг берётся из голов (`dying ∪ drains` в `land`) — клетка
/// контейнера обязана остаться в образе, иначе это утечка на два круга.
#[test]
fn eaten_container_survives_two_rounds() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, 1).await.unwrap();
        let key = b"big";
        let mut ks = Vec::new();
        for _ in 0..8 {
            let k = st.alloc_draft_page().await.unwrap();
            st.write_draft_page(k, |_| (vec![7u8; 3], ()))
                .await
                .unwrap();
            ks.push(k);
        }
        st.put_cache(0, key, &[0], &ks, &[]).await.unwrap();
        st.flush().await.unwrap();
        let (g, cv) = st.get_cache(0, key).await.unwrap().expect("ключ");
        let old = cv.pages;
        drop(g);
        let k2 = st.alloc_draft_page().await.unwrap();
        st.write_draft_page(k2, |_| (vec![1, 2, 3], ()))
            .await
            .unwrap();
        st.put_cache(0, key, &[0], &[k2], &old).await.unwrap();
        st.flush().await.unwrap(); // смерти — клетками в цепи
        // вычерпать головы аллокациями — сплайсы едят чанки, контейнеры
        // оседают в spent
        for _ in 0..40 {
            st.alloc_draft_page().await.unwrap();
        }
        st.flush().await.unwrap(); // контейнеры — клетками в чанки круга
        st.flush().await.unwrap(); // фронт суперседится — клетки живут?
        st.partition_exact()
            .await
            .expect("клетки съеденных контейнеров");
    });
}

// ## Восстановленный остаток хвоста

/// Неприкосновенный восстановленный вход с off≠0 + новые чанки того же
/// пула. Клетки пула (смерти своей чётности) не зависят от поедания
/// хвоста: последний новый чанк получает next = нетронутый вход ⟨C,
/// off≠0⟩ — ассерт init_chain («продолжение за next — целая страница»)
/// нарушается и паникует (assert_eq! жив и в release). Для модели
/// состояние легально: chainWrites пишет entry с off в next, формат
/// модели хранит off в заголовке chunk'а. Репро: образ со входом ⟨2,
/// off 2⟩, ни одного alloc (хвост не тронут), смерть чётной страницы —
/// и flush.

#[test]
fn recovered_tail_behind_new_chunk() {
    run(async {
        let disk = MemDisk::default();
        offset_image(&disk).await;
        let st = Storage::open(&disk, "db", 4, 1).await.unwrap();
        st.put_cache(0, b"k", &[0], &[], &[8]).await.unwrap();
        {
            let (g, _) = st.get_cache(0, b"k").await.unwrap().expect("ключ");
            drop(g); // пин-цикл: смерть осаждается в кучу
        }
        st.flush().await.unwrap(); // здесь паника
        assert!(st.ok_store().await);
        drop(st);
        let st2 = Storage::open(&disk, "db", 4, 1).await.unwrap();
        let (g, cv) = st2.get_cache(0, b"k").await.unwrap().expect("ключ");
        drop(g);
        assert_eq!(cv.mt, vec![0]);
        assert!(st2.ok_store().await);
    });
}

/// Таблица — B-tree, и круги обязаны её держать: сплошная укладка,
/// потом прививка в середину полных листьев, под левый край и поверх
/// правого с расколом корня (жирные записи — по две на лист — 400
/// листьев ломают байтовый предел интерьера). Все ключи переживают
/// flush и рестарт, чистые страницы остаются при своих поддеревьях.
#[test]
fn tree_holds_many_keys() {
    run(async {
        async fn put(st: &Storage<MemDisk>, key: &[u8], mt: &[u8]) {
            let p = st.alloc_draft_page().await.unwrap();
            let k = key.to_vec();
            st.write_draft_page(p, move |_| (k.clone(), ()))
                .await
                .unwrap();
            let (k, m) = (key.to_vec(), mt.to_vec());
            st.put_cache(0, &k, &m, &[p], &[]).await.unwrap();
        }
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, 1).await.unwrap();
        // круг 1 — нечётные 1..599: сплошная укладка полного дерева
        for r in 0..300u16 {
            let mut k = vec![1u8];
            k.extend_from_slice(&(2 * r + 1).to_le_bytes());
            put(&st, &k, &[]).await;
        }
        st.flush().await.unwrap();
        // круг 2 — чётные 0..598: прививка в середину полных листьев
        for r in 0..300u16 {
            let mut k = vec![1u8];
            k.extend_from_slice(&(2 * r).to_le_bytes());
            put(&st, &k, &[]).await;
        }
        st.flush().await.unwrap();
        // круг 3 — ниже наименьшего: прививка под левый край
        for r in 0..50u16 {
            let mut k = vec![0u8];
            k.extend_from_slice(&r.to_le_bytes());
            put(&st, &k, &[]).await;
        }
        st.flush().await.unwrap();
        // круг 4 — жирные записи поверх правого края: 400 листьев,
        // раскол корня
        let fat = vec![7u8; 2000];
        for r in 0..800u16 {
            let mut k = vec![2u8];
            k.extend_from_slice(&r.to_le_bytes());
            st.put_cache(0, &k, &fat, &[], &[]).await.unwrap();
        }
        st.flush().await.unwrap();
        drop(st);
        let st2 = Storage::open(&disk, "db", 4, 1).await.unwrap();
        let mut expect: Vec<Vec<u8>> = (0..50u16)
            .map(|r| [0u8, r.to_le_bytes()[0], r.to_le_bytes()[1]].to_vec())
            .chain((0..600u16).map(|r| [1u8, r.to_le_bytes()[0], r.to_le_bytes()[1]].to_vec()))
            .chain((0..800u16).map(|r| [2u8, r.to_le_bytes()[0], r.to_le_bytes()[1]].to_vec()))
            .collect();
        expect.sort_unstable();
        for k in &expect {
            let (g, cv) = st2
                .get_cache(0, k)
                .await
                .unwrap()
                .unwrap_or_else(|| panic!("ключ {k:?} потерян"));
            if k[0] == 2 {
                assert_eq!(cv.mt.len(), 2000, "ключ {k:?}: жирная запись инлайн");
                assert!(cv.pages.is_empty());
            } else {
                assert_eq!(cv.pages.len(), 1, "ключ {k:?}: одна страница значения");
            }
            drop(g);
        }
        st2.partition_exact().await.expect("биекция");
    });
}
