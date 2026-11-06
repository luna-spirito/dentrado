//! Нагрузка поверх сценариев: рукодельный образ со входом-смещением
//! (семантика «off живёт во входе»), случайная нагрузка с теневой
//! моделью, крошечный пул слотов, B-tree под кругами flush'а.
//! Многочанковые цепочки и смерти под чужим пином — в [`chunks`].

use std::collections::HashMap;

use dentrado_cache::{MemDisk, Storage};

mod util;
use util::{del, offset_image, put, run, specs};

// ## Вход со смещением: off — атрибут входа, не страницы

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
        let mut s = st.session();
        let mut ks = Vec::new();
        for _ in 0..3 {
            ks.push(s.alloc().await.unwrap());
        }
        assert_eq!(
            st.debug_state().free_nexts,
            [14, 15],
            "три страницы — из цепочки, не ростом мира"
        );
        for k in ks {
            s.discard(k).unwrap();
        }
        drop(s);
        let mut head = st.debug_state().heads[0].clone();
        head.sort_unstable();
        assert_eq!(head, vec![8, 10, 12], "мёртвые слоты пропущены");
        assert!(st.ok_store().await);
    });
}

// ## Случайная нагрузка с теневой моделью

/// 8 ключей, 260 операций: публикации 0..2500 байт (инлайн), чтения
/// со сверкой, удаления, flush'ы. Финал — flush, сверка, перезапуск,
/// полная сверка, инвариант.
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
                    put(&st, b"t0", &[key], &payload).unwrap();
                    shadow.insert(key, payload);
                }
                12..=16 => {
                    let got = st.get(b"t0", &[key]).await.unwrap();
                    let want = shadow.get(&key).cloned();
                    assert_eq!(got, want, "op {op}, ключ {key}");
                }
                17..=18 => {
                    del(&st, b"t0", &[key]).unwrap();
                    shadow.remove(&key);
                }
                _ => {
                    st.flush().await.unwrap();
                }
            }
        }
        st.flush().await.unwrap();
        let dbg = st.debug_state();
        // финальный круг перестроил таблицу: старые страницы дерева —
        // в куче EBR (сколько именно — форма старого дерева, не договор)
        assert!(
            dbg.dying_len >= 1,
            "старое дерево финального круга: {dbg:?}"
        );
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
            put(&st, b"t0", &[r], &[r; 100]).unwrap();
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
        // круг 1 — нечётные 1..599: сплошная укладка полного дерева
        for r in 0..300u16 {
            put(&st, b"t0", &odd(1, 2 * r + 1), b"").unwrap();
        }
        st.flush().await.unwrap();
        // круг 2 — чётные 0..598: прививка в середину полных листьев
        for r in 0..300u16 {
            put(&st, b"t0", &odd(1, 2 * r), b"").unwrap();
        }
        st.flush().await.unwrap();
        // круг 3 — ниже наименьшего: прививка под левый край
        for r in 0..50u16 {
            put(&st, b"t0", &odd(0, r), b"").unwrap();
        }
        st.flush().await.unwrap();
        // круг 4 — жирные записи поверх правого края: 500 байт инлайн,
        // раскол корня
        let fat = vec![7u8; 500];
        for r in 0..800u16 {
            put(&st, b"t0", &odd(2, r), &fat).unwrap();
        }
        st.flush().await.unwrap();
        drop(st);
        let st2 = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        let mut expect: Vec<Vec<u8>> = (0..50u16)
            .map(|r| odd(0, r))
            .chain((0..600u16).map(|r| odd(1, r)))
            .chain((0..800u16).map(|r| odd(2, r)))
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

/// Ключ `tag ‖ LE(n)`.
fn odd(tag: u8, n: u16) -> Vec<u8> {
    let mut k = vec![tag];
    k.extend_from_slice(&n.to_le_bytes());
    k
}
