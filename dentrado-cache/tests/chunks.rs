//! Многочанковые цепочки и смерти: цепь ≥2 chunk-страниц на пул,
//! смерти под чужим пином (pending-бакет EBR — читатель паркуется
//! посреди спуска, убийца печатает), их переживание краша, разбиение
//! страниц в покое, контейнер съеденного чанка через два круга и
//! поедание восстановленного остатка (off≠0-вход). Большие значения
//! едут тестовыми чанками (см. [`util`]): чанкирование — забота
//! userspace.

use std::task::Poll;

use dentrado_cache::{MemDisk, Rec, Storage};

mod util;
use util::{PAYLOAD, chain_specs, offset_image, put_big, read_big, run, specs};

// ## Многочанковые цепочки

/// Цепь на ~1100 страниц, публикация, затем смерть: смерти уходят
/// в цепочки из ≥2 chunk-страниц (511 слотов); инвариант против байтов
/// проверяет счётчики и суммы обеих цепочек, восстановление раздаёт
/// запас из цепочек, пере-публикация ест их заново.
#[test]
fn multi_chunk_flush() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, &chain_specs(&[b"t0"]))
            .await
            .unwrap();
        let key = b"big";
        let big = vec![7u8; 1100 * PAYLOAD];
        let chain = put_big(&st, b"t0", key, &big, &[]).await.unwrap();
        st.flush().await.unwrap();
        assert!(st.ok_store().await);
        assert_eq!(read_big(&st, b"t0", key).await.unwrap(), big);
        // замена на инлайн: 1100 умирают списком звонящего — в цепи
        // своего пула (свежий свободный запас: клетки пишутся один раз,
        // дальше их только едят — голова пула остаётся пустой)
        st.put_many(
            b"t0",
            &[Rec::Put {
                key,
                val: &[1, 2, 3],
                take: &[],
                free: &chain,
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
        let st2 = Storage::open(&disk, "db", 4, &chain_specs(&[b"t0"]))
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
        let big2 = vec![9u8; 5 * PAYLOAD];
        put_big(&st2, b"t0", b"two", &big2, &[]).await.unwrap();
        st2.flush().await.unwrap();
        assert!(st2.ok_store().await, "выедание цепочки с дренажом");
    });
}

// ## Смерти в pending-бакете: расхождение с моделью
//
// Порт: гварды ушли внутрь поверхности — пин живёт лишь внутри одного
// вызова. Расхождение с моделью остаётся для смертей под чужим пином:
// читатель паркуется посреди спуска (кэш в одну страницу вымывает
// всё), убийца заменяет запись и печатает — оборот руки невозможен,
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
        let st = Storage::open(&disk, "db", 1, &chain_specs(&[b"t0"]))
            .await
            .unwrap();
        let key = b"big";
        // модель мира: цепь выросла из чётного пула (2, 4, …, 12),
        // дерево первого круга — одна страница нечётного (3)
        let blob = put_big(&st, b"t0", key, &[0u8; 6 * PAYLOAD], &[])
            .await
            .unwrap();
        st.flush().await.unwrap();
        assert_eq!(
            blob,
            (0..6).map(|i| 2 + 2 * i as u64).collect::<Vec<_>>(),
            "страницы цепи — чётный пул"
        );
        let leaf = 3u64;
        {
            // один опрос читателя: пин взят, спуск паркуется на чтении
            // листа; смерть и печать — под живым пином: оборот руки
            // невозможен, смерти сидят в pending-бакете и раунд
            // пропускают. (Блок, а не drop(reader): pin! одалживает
            // скрытое будущее — бросание ссылки его не роняет, роняет
            // только выход из области.)
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
            // замена: цепь умирает списком звонящего, старый лист —
            // прививкой; обе партии — под чужим пином
            st.put_many(
                b"t0",
                &[Rec::Put {
                    key,
                    val: &[1, 2, 3],
                    take: &[],
                    free: &blob,
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
        } // уход читателя осаждает кучу: шесть страниц цепи и старое
        // дерево круга (оно тоже сидело в pending — пин защищал
        // читателя посреди спуска)
        assert_eq!(st.debug_state().dying_len, blob.len() + 1, "дренаж в кучу");
        st.flush().await.unwrap();
        let dbg = st.debug_state();
        // чётные смерти — чётности задачного пула: голова; нечётный
        // лист — чётности печатного: едет цепью (клетка глубокого
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
        let st = Storage::open(&disk, "db", 1, &chain_specs(&[b"t0"]))
            .await
            .unwrap();
        let key = b"k";
        let blob = put_big(&st, b"t0", key, &[0u8; 4 * PAYLOAD], &[])
            .await
            .unwrap();
        st.flush().await.unwrap();
        // читатель паркуется посреди спуска — его пин пережил бы печать;
        // здесь он брошен сразу: снятие пина
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
        } // бросание читателя посреди спуска — снятие его пина
        st.put_many(
            b"t0",
            &[Rec::Put {
                key,
                val: &[9; 3],
                take: &[],
                free: &blob,
            }],
        )
        .await
        .unwrap();
        st.flush().await.unwrap(); // смерти — под чужим пином: pending
        drop(st); // краш: пины и арена исчезли, образ — как закоммичен
        let st2 = Storage::open(&disk, "db", 4, &chain_specs(&[b"t0"]))
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
            st.write_draft_page(k, move |pg| pg.0[..3].copy_from_slice(&[i; 3]))
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
        let st = Storage::open(&disk, "db", 4, &chain_specs(&[b"t0"]))
            .await
            .unwrap();
        let mut old: Vec<dentrado_cache::PageId> = Vec::new();
        for r in 0..3u8 {
            let big = vec![r; 6 * PAYLOAD];
            old = put_big(&st, b"t0", b"big", &big, &old).await.unwrap();
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
        let st = Storage::open(&disk, "db", 4, &chain_specs(&[b"t0"]))
            .await
            .unwrap();
        let big = vec![7u8; 8 * PAYLOAD];
        let old = put_big(&st, b"t0", b"big", &big, &[]).await.unwrap();
        st.flush().await.unwrap();
        st.put_many(
            b"t0",
            &[Rec::Put {
                key: b"big",
                val: &[1, 2, 3],
                take: &[],
                free: &old,
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
        let st = Storage::open(&disk, "db", 4, &chain_specs(&[b"t0"]))
            .await
            .unwrap();
        // одночанковая публикация берёт голову чётного пула — слот 8;
        // хвост (вход ⟨2, off 2⟩) не тронут
        let old = put_big(&st, b"t0", b"k", &[7u8; 600], &[]).await.unwrap();
        // замена на инлайн: слот 8 умирает списком звонящего
        st.put_many(
            b"t0",
            &[Rec::Put {
                key: b"k",
                val: &[9; 3],
                take: &[],
                free: &old,
            }],
        )
        .await
        .unwrap();
        st.flush().await.unwrap(); // здесь паника
        assert!(st.ok_store().await);
        drop(st);
        let st2 = Storage::open(&disk, "db", 4, &chain_specs(&[b"t0"]))
            .await
            .unwrap();
        assert_eq!(st2.get(b"t0", b"k").await.unwrap(), Some(vec![9; 3]));
        assert!(st2.ok_store().await);
    });
}
