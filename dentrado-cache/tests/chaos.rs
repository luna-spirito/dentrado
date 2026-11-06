//! Ужесточение поверх сценариев: строгая биекция страниц
//! (partition_exact) после каждой фазы, отмена фьючерсов в await-точках
//! (штатный режим вызова под select/timeout), нагрузка × transient-сбои,
//! краш с порванной записью в полёта.

mod common;
mod util;

use std::collections::HashMap;
use std::rc::Rc;
use std::task::Poll;

use common::{expect_turns, race_turns};
use dentrado_cache::{Fault, MemDisk, Rec, Storage, StoreError};
use util::{PAYLOAD, chain_specs, put, put_big, run, specs};

const KEY: &[u8] = b"k";
const KEY2: &[u8] = b"j";

/// Публикация значения из трёх байт.
async fn put_v(st: &Storage<MemDisk>, key: &[u8], val: u8) {
    st.put_many(b"t0", &[put(key, &[val; 3])]).await.unwrap();
}

/// Прочитать первый байт значения (со сверкой по теневой версии).
async fn first_byte(st: &Storage<MemDisk>, key: &[u8]) -> Option<u8> {
    st.get(b"t0", key).await.unwrap().map(|v| v[0])
}

/// Страницы живой цепи ключа — по `mt`, ходом `next`-заголовков, без
/// чтения содержимого: список для `free`.
async fn chain_pages(st: &Storage<MemDisk>, key: &[u8]) -> Vec<dentrado_cache::PageId> {
    let mt = st.get(b"t0", key).await.unwrap().expect("ключ есть");
    let mut head = u64::from_le_bytes(mt[..8].try_into().unwrap());
    let mut ps = Vec::new();
    while head != 0 {
        ps.push(head);
        head = st
            .read_page(head, |pg| u64::from_le_bytes(pg.0[..8].try_into().unwrap()))
            .await
            .unwrap();
    }
    ps
}

/// Харнесс жив: обычные операции проходят под бюджетом оборотов, биекция
/// чиста — иначе всё красное ниже было бы артефактом харнесса.
#[test]
fn harness_sanity() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        expect_turns(put_v(&st, KEY, 1), 256, "put_v").await;
        expect_turns(st.flush(), 512, "flush").await.unwrap();
        st.partition_exact().await.expect("биекция после круга");
        drop(st);
        let st2 = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        st2.partition_exact().await.expect("биекция после рестарта");
    });
}

// ## Отмена фьючерсов в await-точках

/// Отмена flush'а в произвольной await-точке безвредна: круг крутит
/// агент, фьючерс вызывающего — лишь письмо и ожидание ответа; отмена
/// роняет адрес ответа, круг доходит до конца (гарантия ревьи
/// 2026-10-20 #5, сторона круга).
#[test]
fn flush_cancel_recovers() {
    run(async {
        for k in 0..48u32 {
            let disk = MemDisk::default();
            let st = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
                .await
                .unwrap();
            put_v(&st, KEY, 1).await;
            let _ = race_turns(st.flush(), k).await; // отмена в точке №k
            expect_turns(st.flush(), 512, &format!("flush после отмены на {k}"))
                .await
                .unwrap();
            st.partition_exact()
                .await
                .unwrap_or_else(|v| panic!("k={k}: {v:?}"));
        }
    });
}

/// Провал хвостового чтения не клинит пул: хвост обязан вернуться в
/// Free (Busy → Free отката нет), следующий alloc — пройти. Репро по
/// мотивам ревьи 2026-10-20 #7: `?` в alloc бросает Busy-хвост, все
/// последующие аллоки паркуются навсегда.

#[test]
fn alloc_cancel_recovers() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, &chain_specs(&[b"t0"]))
            .await
            .unwrap();
        // 1100 смертей (рецепт multi_chunk_flush): запас уезжает в цепь
        // своего пула, головы пусты — первый аллок обязан читать чанк
        let key = b"tail";
        let big = vec![1u8; 1100 * PAYLOAD];
        put_big(&st, b"t0", key, &big, &[]).await.unwrap();
        st.flush().await.unwrap();
        // замена на инлайн: 1100 страниц цепи — во free
        let pages = chain_pages(&st, key).await;
        st.put_many(
            b"t0",
            &[Rec::Put {
                key,
                val: &[9; 3],
                take: &[],
                free: &pages,
            }],
        )
        .await
        .unwrap();
        st.flush().await.unwrap();
        drop(st);
        // рестарт с кэшем в одну страницу: проход загрузки вымывает чанк
        // из резидента, первый аллок обязан лететь в диск; Fault валит
        // его, `?` бросает хвост в Busy
        let st = Storage::open(&disk, "db", 1, &chain_specs(&[b"t0"]))
            .await
            .unwrap();
        disk.file("db").set_fault(Fault::At(1));
        let e = st.alloc_draft_page().await;
        disk.file("db").set_fault(Fault::None);
        assert!(
            matches!(e, Err(StoreError::Io(_))),
            "хвостовое чтение обязано было пасть: {e:?}"
        );
        expect_turns(
            st.alloc_draft_page(),
            512,
            "аллок после провала чтения хвоста",
        )
        .await
        .unwrap();
        st.partition_exact().await.expect("биекция");
    });
}

/// Отмена алока посреди хвостового чтения (не io-ошибка — дроп
/// фьючерса в await-точке): переход Free→Busy уже сделан в секции,
/// splice прибудет разве что от читателя, а читателя выбросили.
/// Тот же дефект ревьи 2026-10-20 #7, второй вход: k=0 отменяет аллок
/// в первом же опросе — сразу после секции, до всякого диска.

#[test]
fn alloc_reader_cancel_recovers() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, &chain_specs(&[b"t0"]))
            .await
            .unwrap();
        let key = b"tail";
        let big = vec![1u8; 1100 * PAYLOAD];
        put_big(&st, b"t0", key, &big, &[]).await.unwrap();
        st.flush().await.unwrap();
        // замена на инлайн: цепь умирает списком звонящего
        let pages = chain_pages(&st, key).await;
        st.put_many(
            b"t0",
            &[Rec::Put {
                key,
                val: &[9; 3],
                take: &[],
                free: &pages,
            }],
        )
        .await
        .unwrap();
        st.flush().await.unwrap();
        drop(st);
        // рестарт с кэшем в одну страницу: головы пусты, хвост чётного
        // пула — на диске; первый опрос алока делает Free→Busy и паркует
        // на чтении — дроп. Хвост обязан вернуться в Free.
        let st = Storage::open(&disk, "db", 1, &chain_specs(&[b"t0"]))
            .await
            .unwrap();
        assert!(race_turns(st.alloc_draft_page(), 0).await.is_none());
        expect_turns(st.alloc_draft_page(), 512, "аллок после отмены читателя")
            .await
            .unwrap();
        st.partition_exact().await.expect("биекция");
    });
}

/// Смерть ждуна безвредна: его письмо в ящике агента несёт уже
/// мёртвый адрес ответа — агент всё равно гонит батч в один круг,
/// send тонет молча, следующий flush звонит в колокол и получает свой
/// круг (гарантия ревьи 2026-10-20 #5, сторона ждуна).
#[test]
fn flush_waiter_cancel_recovers() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        put_v(&st, KEY, 1).await;
        // ждун №1 — живой пиннед фьючерс: один опрос кладёт письмо и
        // паркует на ответе; круг крутится в агенте
        let mut w = std::pin::pin!(st.flush());
        let mut n = 1u32;
        futures::future::poll_fn(|cx| {
            if n == 0 {
                Poll::Ready(())
            } else {
                n -= 1;
                let _ = w.as_mut().poll(cx);
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await;
        // ждун №2: один опрос — письмо в ящик, и отброс (адрес умер)
        assert!(race_turns(st.flush(), 0).await.is_none());
        w.await.unwrap(); // круг дошёл, ответ №2 утонул в send
        expect_turns(st.flush(), 512, "flush после смерти ждуна")
            .await
            .unwrap();
        st.partition_exact().await.expect("биекция");
    });
}

/// Отмена чтения безвредна: пины — внутренние RAII, биекция чиста.
#[test]
fn get_cancel_is_harmless() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        put_v(&st, KEY, 1).await;
        st.flush().await.unwrap();
        for j in 0..16u32 {
            let _ = race_turns(st.get(b"t0", KEY), j).await;
        }
        let v = expect_turns(st.get(b"t0", KEY), 256, "get после отмен")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(v, vec![1u8; 3]);
        st.partition_exact().await.expect("биекция");
    });
}

// ## Transient-сбой: стор обязан пережить и не потерять страницы

/// Один отказ записи посреди круга flush'а: стор обязан продолжать
/// работать, следующий круг — закоммитить мир, в котором каждая страница
/// ниже счётчиков имеет ровно одну роль. Ревья 2026-10-20 #1: круг без
/// обратного глагола (unseal) теряет запечатанный мир навсегда —
/// подтверждено: после transient-отказа и следующего коммита страница
/// смерти выпадает из образа (Missing).

#[test]
fn transient_flush_failure_keeps_partition() {
    run(async {
        for f in 1..40u64 {
            let disk = MemDisk::default();
            let st = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
                .await
                .unwrap();
            put_v(&st, KEY, 1).await;
            st.flush().await.unwrap();
            st.put_many(b"t0", &[put(KEY, &[2u8; 3])]).await.unwrap();
            disk.file("db").set_fault(Fault::Once(f));
            let _ = st.flush().await; // transient отказ пережит
            disk.file("db").set_fault(Fault::None);
            // стор обязан продолжать работать (как того требует сценарий
            // concurrent_workers) и закоммитить честный мир
            put_v(&st, KEY2, 3).await;
            st.flush().await.unwrap();
            st.partition_exact()
                .await
                .unwrap_or_else(|v| panic!("f={f}: {v:?}"));
            drop(st);
            let st2 = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
                .await
                .unwrap();
            st2.partition_exact()
                .await
                .unwrap_or_else(|v| panic!("f={f}, после рестарта: {v:?}"));
        }
    });
}

// ## Краш с порванной записью в полёте

/// Ошибки чтения обоих SB-слотов — не eof: поверх живого образа нельзя
/// грузиться свежим (аллокация начнёт с страницы 2, следующий flush
/// начнёт разрушать старый мир). Ревья 2026-10-20 #8: `sb_slot` валит
/// eof и io в одну кучу — подтверждено: open молча возвращает свежий
/// стор, ключ пропал (None), без всякой ошибки.

#[test]
fn read_error_does_not_wipe_image() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        put_v(&st, KEY, 1).await;
        st.flush().await.unwrap();
        drop(st);
        disk.file("db").set_fault(Fault::At(1)); // «transient EIO на чтениях»
        let opened = Storage::open(&disk, "db", 4, &specs(&[b"t0"])).await;
        disk.file("db").set_fault(Fault::None);
        assert!(opened.is_err(), "молчаливая загрузка поверх живого образа");
    });
}

/// Порванная запись (полстраницы новой, полстарой) и смерть всего после:
/// восстановление видит старый или новый образ — исход строго бинарный,
/// восстановленный мир — строгая биекция.
#[test]
fn torn_write_recovery_is_binary() {
    run(async {
        for f in 1..40u64 {
            let disk = MemDisk::default();
            let st = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
                .await
                .unwrap();
            put_v(&st, KEY, 1).await;
            st.flush().await.unwrap();
            st.put_many(b"t0", &[put(KEY, &[2u8; 3])]).await.unwrap();
            disk.file("db").set_fault(Fault::Torn(f));
            let _ = st.flush().await; // запись в полёте порвана, далее — смерть
            drop(st); // краш
            disk.file("db").set_fault(Fault::None); // новый процесс — диск здоров
            match Storage::open(&disk, "db", 4, &specs(&[b"t0"])).await {
                Ok(st2) => {
                    let out = first_byte(&st2, KEY).await;
                    assert!(out == Some(1) || out == Some(2), "f={f}: {out:?}");
                    st2.partition_exact()
                        .await
                        .unwrap_or_else(|v| panic!("f={f}: {v:?}"));
                }
                Err(StoreError::CorruptImage) => {}
                Err(e) => panic!("f={f}: {e}"),
            }
        }
    });
}

// ## Нагрузка с конкурентными кругами и dealloc'ами

/// Работники (публикации, замены со смертью, alloc→dealloc — страницы
/// домой посреди круга, чтения со сверкой) и конкурентные flush'ы; после
/// дренажа — финальный круг, строгая биекция, рестарт, сверка и снова
/// биекция.
async fn worker(st: Rc<Storage<MemDisk>>, kb: u8, mut seed: u64, rounds: u8) -> u8 {
    let mut rnd = move || {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        seed
    };
    let key = [kb];
    let mut ver = 0u8;
    for _ in 0..rounds {
        match rnd() % 4 {
            0..=1 => {
                let v = ver.wrapping_add(1);
                let cell = vec![kb, v];
                st.put_many(b"t0", &[put(&key, &cell)]).await.unwrap();
                ver = v;
            }
            2 => {
                // черновик в никуда: страница домой, посреди круга в том числе
                let k = st.alloc_draft_page().await.unwrap();
                st.write_draft_page(k, |pg| pg.0[0] = 0).await.unwrap();
                st.dealloc_draft_page(k).await.unwrap();
            }
            _ => {
                if let Some(b) = st.get(b"t0", &key).await.unwrap() {
                    assert_eq!(b, vec![kb, ver], "чтение работника {kb}");
                }
            }
        }
    }
    ver
}

#[test]
fn chaos_workers_partition() {
    run(async {
        let disk = MemDisk::default();
        let st = Rc::new(
            Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
                .await
                .unwrap(),
        );
        let mut ts = Vec::new();
        for (kb, seed) in [(5u8, 0x1234u64), (9u8, 0x5678), (13u8, 0x9abc)] {
            let s = st.clone();
            ts.push((kb, compio::runtime::spawn(worker(s, kb, seed, 40))));
        }
        let mut expected: HashMap<u8, u8> = HashMap::new();
        for (kb, t) in ts {
            expected.insert(kb, t.await.unwrap());
            let s = st.clone();
            // конкурентный flush может честно отказать — финальный обязан
            let _ = compio::runtime::spawn(async move { s.flush().await }).await;
        }
        st.flush().await.unwrap();
        st.partition_exact().await.expect("биекция после дренажа");
        drop(st);
        let st2 = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        for (kb, v) in &expected {
            let b = st2.get(b"t0", &[*kb]).await.unwrap().expect("ключ потерян");
            assert_eq!(b[1], *v, "работник {kb} после рестарта");
        }
        st2.partition_exact().await.expect("биекция после рестарта");
    });
}
