//! Нагрузка поверх сценариев: рукодельный образ со входом-смещением
//! (семантика «off живёт во входе»), многочанковый flush (цепочки
//! ≥2 chunk-страниц на пул), случайная нагрузка с теневой моделью,
//! крошечный пул слотов, расхождение смертей с моделью (pending-бакет
//! EBR под чужим пином) и поедание восстановленного остатка (off≠0-вход)
//! кругом flush'а.

use std::collections::HashMap;
use std::task::Poll;

use dentrado_cache::core::freelist::{ChainEntry, write_chunk};
use dentrado_cache::core::sb::write_sb;
use dentrado_cache::core::val::VAL_PAYLOAD;
use dentrado_cache::page::{Page, fnv_id};
use dentrado_cache::{IsFilesystem, MemDisk, Rec, Storage, TableSpec};

fn run<F: Future>(f: F) -> F::Output {
    compio::runtime::Runtime::new().unwrap().block_on(f)
}

fn specs(ids: &[&[u8]]) -> Vec<TableSpec> {
    ids.iter().map(|i| TableSpec::flat(i.to_vec())).collect()
}

/// Значение ровно на `n` цепочных страниц.
fn chain_val(n: usize, fill: u8) -> Vec<u8> {
    vec![fill; n * VAL_PAYLOAD]
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
        &[(b"t0".to_vec(), 0)],
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

        let st = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
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
        let st = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        let key = b"big";
        let big = chain_val(1100, 7);
        st.put_many(b"t0", &[Rec::Put { key, val: &big }])
            .await
            .unwrap();
        st.flush().await.unwrap();
        assert!(st.ok_store().await);
        assert_eq!(st.get(b"t0", key).await.unwrap(), Some(big));
        // замена на инлайн: 1100 умирают замыканием — в цепи своего пула
        // (свежий свободный запас: клетки пишутся один раз, дальше их
        // только едят — голова пула остаётся пустой)
        st.put_many(
            b"t0",
            &[Rec::Put {
                key,
                val: &[1, 2, 3],
            }],
        )
        .await
        .unwrap();
        // пинов снаружи нет — reg сам осаждает свою партию, печать забирает
        // смерти без пин-цикла (случай под чужим пином — death_pending_bucket)
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
        let st2 = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        assert_eq!(st2.get(b"t0", key).await.unwrap(), Some(vec![1, 2, 3]));
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
        let big2 = chain_val(5, 9);
        st2.put_many(
            b"t0",
            &[Rec::Put {
                key: b"two",
                val: &big2,
            }],
        )
        .await
        .unwrap();
        st2.flush().await.unwrap();
        assert!(st2.ok_store().await, "выедание цепочки с дренажом");
    });
}

// ## Случайная нагрузка с теневой моделью

/// 8 ключей, 260 операций: публикации 0..2500 байт (инлайн и цепочки),
/// чтения со сверкой, удаления, flush'ы. Финал — flush, сверка,
/// перезапуск, полная сверка, инвариант.
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
        let st = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        let mut shadow: HashMap<u8, Vec<u8>> = HashMap::new();
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
                    st.put_many(
                        b"t0",
                        &[Rec::Put {
                            key: &[key],
                            val: &payload,
                        }],
                    )
                    .await
                    .unwrap();
                    shadow.insert(key, payload);
                }
                12..=16 => {
                    let got = st.get(b"t0", &[key]).await.unwrap();
                    let want = shadow.get(&key).cloned();
                    assert_eq!(got, want, "op {op}, ключ {key}");
                }
                17..=18 => {
                    st.put_many(b"t0", &[Rec::Del { key: &[key] }])
                        .await
                        .unwrap();
                    shadow.remove(&key);
                }
                _ => {
                    st.flush().await.unwrap();
                }
            }
        }
        st.flush().await.unwrap();
        let dbg = st.debug_state();
        // финальный круг перестроил таблицу: старое дерево (одна
        // страница — ключей мало) — в куче EBR
        assert_eq!(dbg.dying_len, 1, "старое дерево финального круга: {dbg:?}");
        assert!(st.ok_store().await);
        st.partition_exact().await.expect("биекция финала");
        drop(st);

        let st2 = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        for key in 0..8u8 {
            assert_eq!(
                st2.get(b"t0", &[key]).await.unwrap(),
                shadow.get(&key).cloned(),
                "ключ {key} после рестарта"
            );
        }
        assert!(st2.ok_store().await);
        st2.partition_exact().await.expect("биекция после рестарта");
    });
}

// ## Крошечный пул

/// Два слота арены: вытеснение на каждом шаге — записи, чтения и
/// сохранения чередуются на грани емкости.
#[test]
fn tiny_pool_stress() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 2, &specs(&[b"t0"]))
            .await
            .unwrap();
        for r in 0..40u8 {
            st.put_many(
                b"t0",
                &[Rec::Put {
                    key: &[r],
                    val: &[r; 100],
                }],
            )
            .await
            .unwrap();
        }
        st.flush().await.unwrap();
        assert!(st.ok_store().await);
        for r in 0..40u8 {
            assert_eq!(st.get(b"t0", &[r]).await.unwrap(), Some(vec![r; 100]));
        }
        drop(st);
        let st2 = Storage::open(&disk, "db", 2, &specs(&[b"t0"]))
            .await
            .unwrap();
        assert_eq!(st2.get(b"t0", &[7]).await.unwrap(), Some(vec![7; 100]));
        assert!(st2.ok_store().await);
    });
}

// ## Смерти в pending-бакете: расхождение с моделью
//
// Порт: гварды ушли внутрь поверхности — пин живёт лишь внутри одного
// вызова. Расхождение с моделью остаётся для смертей под чужим пином:
// читатель большой цепочки (кэш в одну страницу) паркуется посреди
// обхода, убийца заменяет запись и печатает — оборот руки невозможен,
// смерти сидят в pending-бакете и раунд пропускают. Уход читателя
// осаждает кучу, вторая печать догоняет. Модель держит плоский список
// смертей, и та же самая печать обязана его забрать. Цена расхождения
// — окно: умри процесс после коммита новой записи, но до дренажа, и
// страницы выпадают из образа — поэтому печать пакует pendings клетками
// (лимбо), что и проверяет pending_deaths_survive_crash ниже.

#[test]
fn death_pending_bucket() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 1, &specs(&[b"t0"]))
            .await
            .unwrap();
        let key = b"big";
        let big = chain_val(6, 0);
        st.put_many(b"t0", &[Rec::Put { key, val: &big }])
            .await
            .unwrap();
        st.flush().await.unwrap();
        // один опрос читателя: пин взят, обход цепочки паркуется на
        // чтении (кэш в одну страницу вымывает всё); смерть и печать —
        // под живым пином: оборот руки невозможен, смерти сидят в
        // pending-бакете и раунд пропускают. (Блок, а не drop(reader):
        // pin! одалживает скрытое будущее — бросание ссылки его не
        // роняет, роняет только выход из области.)
        // модель мира: блоб вырос из чётного пула (2, 4, …, 12),
        // дерево первого круга — одна страница нечётного (3)
        let blob = (0..6).map(|i| 2 + 2 * i as u64).collect::<Vec<_>>();
        let leaf = 3u64;
        {
            let mut reader = std::pin::pin!(st.get(b"t0", key));
            let mut n = 1u32;
            futures::future::poll_fn(|cx| {
                if n == 0 {
                    Poll::Ready(())
                } else {
                    n -= 1;
                    let _ = reader.as_mut().poll(cx);
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            })
            .await;
            st.put_many(
                b"t0",
                &[Rec::Put {
                    key,
                    val: &[1, 2, 3],
                }],
            )
            .await
            .unwrap();
            st.flush().await.unwrap();
            let dbg = st.debug_state();
            assert_eq!(dbg.dying_len, 0, "{dbg:?}");
            assert!(
                blob.iter()
                    .chain(std::iter::once(&leaf))
                    .all(|p| !dbg.heads[0].contains(p) && !dbg.heads[1].contains(p)),
                "смерти пропустили печать — сидят в pending-бакете: {dbg:?}"
            );
            assert!(st.ok_store().await);
        } // уход читателя осаждает кучу: шесть страниц значения и старое
        // дерево круга (оно тоже сидело в pending — пин защищал
        // читателя посреди спуска)
        assert_eq!(st.debug_state().dying_len, blob.len() + 1, "дренаж в кучу");
        st.flush().await.unwrap();
        let dbg = st.debug_state();
        // чётные смерти — чётности задачного пула: голова; нечётный
        // лист — чётности печатного: едет цепью (клетка глубкого
        // запаса), в головы не попадая
        assert!(
            blob.iter().all(|p| dbg.heads[0].contains(p)),
            "смерти догнаны второй печатью: {dbg:?}"
        );
        assert!(
            !dbg.heads[0].contains(&leaf) && !dbg.heads[1].contains(&leaf),
            "нечётная смерть едет цепью, не головой: {dbg:?}"
        );
        assert!(st.ok_store().await);
    });
}

// ## Смерти под чужим пином через печать: краш не теряет страницы

/// Смерти, ждущие чужой пин на момент печати, обязаны быть упакованы
/// клетками (лимбо) — краш стирает пины, и страницы становятся
/// свободными по образу; без упаковки они выпадают из мира навсегда
/// (ни корень, ни цепи их не знают).
#[test]
fn pending_deaths_survive_crash() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 1, &specs(&[b"t0"]))
            .await
            .unwrap();
        let key = b"k";
        let big = chain_val(4, 0);
        st.put_many(b"t0", &[Rec::Put { key, val: &big }])
            .await
            .unwrap();
        st.flush().await.unwrap();
        // читатель паркуется посреди обхода — его пин переживёт печать
        {
            let mut reader = std::pin::pin!(st.get(b"t0", key));
            let mut n = 1u32;
            futures::future::poll_fn(|cx| {
                if n == 0 {
                    Poll::Ready(())
                } else {
                    n -= 1;
                    let _ = reader.as_mut().poll(cx);
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            })
            .await;
        } // бросание читателя посреди обхода — снятие его пина
        st.put_many(b"t0", &[Rec::Put { key, val: &[9; 3] }])
            .await
            .unwrap();
        st.flush().await.unwrap(); // смерти — под чужим пином: pending
        drop(st); // краш: пины и арена исчезли, образ — как закоммичен
        let st2 = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
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
        let st = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        for i in 0..6u8 {
            let k = st.alloc_draft_page().await.unwrap();
            st.write_draft_page(k, move |_| (vec![i; 3], ()))
                .await
                .unwrap();
        }
        st.flush().await.unwrap(); // черновики живы на печати
        drop(st); // краш
        let st2 = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
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
        let st = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        for r in 0..3u8 {
            let big = chain_val(6, r);
            st.put_many(
                b"t0",
                &[Rec::Put {
                    key: b"big",
                    val: &big,
                }],
            )
            .await
            .unwrap();
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
        let st = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        let big = chain_val(8, 7);
        st.put_many(
            b"t0",
            &[Rec::Put {
                key: b"big",
                val: &big,
            }],
        )
        .await
        .unwrap();
        st.flush().await.unwrap();
        st.put_many(
            b"t0",
            &[Rec::Put {
                key: b"big",
                val: &[1, 2, 3],
            }],
        )
        .await
        .unwrap();
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
/// off 2⟩, хвост не тронут (аллок — только из головы), смерть чётной
/// страницы — и flush.

#[test]
fn recovered_tail_behind_new_chunk() {
    run(async {
        let disk = MemDisk::default();
        offset_image(&disk).await;
        let st = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        // аллок цепочной страницы берёт голову чётного пула — слот 8;
        // хвост (вход ⟨2, off 2⟩) не тронут
        st.put_many(
            b"t0",
            &[Rec::Put {
                key: b"k",
                val: &[7u8; 600],
            }],
        )
        .await
        .unwrap();
        // замена на инлайн: слот 8 умирает замыканием
        st.put_many(
            b"t0",
            &[Rec::Put {
                key: b"k",
                val: &[9; 3],
            }],
        )
        .await
        .unwrap();
        st.flush().await.unwrap(); // здесь паника
        assert!(st.ok_store().await);
        drop(st);
        let st2 = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        assert_eq!(st2.get(b"t0", b"k").await.unwrap(), Some(vec![9; 3]));
        assert!(st2.ok_store().await);
    });
}

/// Таблица — B-tree, и круги обязаны её держать: сплошная укладка,
/// потом прививка в середину полных листьев, под левый край и поверх
/// правого с расколом корня (жирные инлайн-записи — по семь на лист —
/// 115 листьев ломают байтовый предел интерьера). Все ключи переживают
/// flush и рестарт, чистые страницы остаются при своих поддеревьях.
#[test]
fn tree_holds_many_keys() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        async fn put(st: &Storage<MemDisk>, key: &[u8], val: &[u8]) {
            st.put_many(b"t0", &[Rec::Put { key, val }]).await.unwrap();
        }
        // круг 1 — нечётные 1..599: сплошная укладка полного дерева
        for r in 0..300u16 {
            let mut k = vec![1u8];
            k.extend_from_slice(&(2 * r + 1).to_le_bytes());
            put(&st, &k, b"").await;
        }
        st.flush().await.unwrap();
        // круг 2 — чётные 0..598: прививка в середину полных листьев
        for r in 0..300u16 {
            let mut k = vec![1u8];
            k.extend_from_slice(&(2 * r).to_le_bytes());
            put(&st, &k, b"").await;
        }
        st.flush().await.unwrap();
        // круг 3 — ниже наименьшего: прививка под левый край
        for r in 0..50u16 {
            let mut k = vec![0u8];
            k.extend_from_slice(&r.to_le_bytes());
            put(&st, &k, b"").await;
        }
        st.flush().await.unwrap();
        // круг 4 — жирные записи поверх правого края: 500 байт инлайн,
        // раскол корня
        let fat = vec![7u8; 500];
        for r in 0..800u16 {
            let mut k = vec![2u8];
            k.extend_from_slice(&r.to_le_bytes());
            put(&st, &k, &fat).await;
        }
        st.flush().await.unwrap();
        drop(st);
        let st2 = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        let mut expect: Vec<Vec<u8>> = (0..50u16)
            .map(|r| [0u8, r.to_le_bytes()[0], r.to_le_bytes()[1]].to_vec())
            .chain((0..600u16).map(|r| [1u8, r.to_le_bytes()[0], r.to_le_bytes()[1]].to_vec()))
            .chain((0..800u16).map(|r| [2u8, r.to_le_bytes()[0], r.to_le_bytes()[1]].to_vec()))
            .collect();
        expect.sort_unstable();
        for k in &expect {
            let v = st2
                .get(b"t0", k)
                .await
                .unwrap()
                .unwrap_or_else(|| panic!("ключ {k:?} потерян"));
            if k[0] == 2 {
                assert_eq!(v, fat, "ключ {k:?}: жирная запись инлайн");
            } else {
                assert!(v.is_empty(), "ключ {k:?}: пустая запись");
            }
        }
        st2.partition_exact().await.expect("биекция");
    });
}
