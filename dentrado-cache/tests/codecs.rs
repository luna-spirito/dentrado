//! Раунд-трипы кодеков и зажимы против шума.

use dentrado_cache::core::freelist::{
    CHUNK_SLOTS, ChainEntry, chunk_cells, chunk_next, write_chunk,
};
use dentrado_cache::core::sb::{MAX_TABLES, parse_sb, write_sb};
use dentrado_cache::core::tree::{Item, build, leaf_find, parse_node, plan, route, write_node};
use dentrado_cache::page::{AlignedPage, PAGE_SIZE, PageId, TORN_BYTE, fnv_id};

#[test]
fn chunk_roundtrip() {
    let cells: Vec<PageId> = (1..=CHUNK_SLOTS as u64).collect();
    let mut p = AlignedPage::zeroed();
    write_chunk(&mut p, 9, &cells);
    assert_eq!(chunk_next(&p), 9);
    // off — атрибут входа, читатель получает его снаружи
    assert_eq!(chunk_cells(&p, 3), cells[3..].to_vec());
    assert_eq!(chunk_cells(&p, 0).len(), CHUNK_SLOTS);
    // нулевые слоты — добивка, читатель их пропускает; off за краем — пусто
    let mut q = AlignedPage::zeroed();
    write_chunk(&mut q, 0, &[4, 0, 6]);
    assert_eq!(chunk_cells(&q, 0), vec![4, 6]);
    assert!(chunk_cells(&q, 600).is_empty());
}

#[test]
fn node_roundtrip() {
    // порядок записей — на совести укладчика, кодек хранит его как есть
    let es = vec![(b"b".to_vec(), vec![2u8]), (b"a".to_vec(), vec![1u8])];
    let mut p = AlignedPage::zeroed();
    // лист: записи без детей — на проводе ни одного слота под них
    write_node(&mut p, 0, &es, &[]);
    let n = parse_node(&p);
    assert_eq!((n.level, n.ids), (0, Vec::<PageId>::new()));
    assert_eq!(n.es, es);
    // интерьер: маршруты и дети
    let mut q = AlignedPage::zeroed();
    write_node(
        &mut q,
        1,
        &[(b"a".to_vec(), vec![]), (b"m".to_vec(), vec![])],
        &[5, 6],
    );
    let n = parse_node(&q);
    assert_eq!(n.ids, vec![5, 6]);
    assert_eq!(n.es, [(b"a".to_vec(), vec![]), (b"m".to_vec(), vec![])]);
    // шум: гигантский счётчик зажат — и лист, и интерьер по минимальной
    // записи
    let mut junk = AlignedPage::zeroed();
    junk.0[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(parse_node(&junk).es.len(), 511);
    junk.put_u32_at(4, 1);
    assert_eq!(parse_node(&junk).es.len(), 255);
}

#[test]
fn tree_build_shape() {
    // 6000 записей: листья ~400 записей — уровней ≥ 2; вход отсортирован
    // по ключам-байтам (порядок — на совести укладчика)
    let mut items: Vec<Item> = (0..6000u32)
        .map(|i| (i.to_le_bytes().to_vec(), vec![(i % 251) as u8]))
        .collect();
    items.sort_by(|a, b| a.0.cmp(&b.0));
    let need: usize = plan(&items).unwrap().iter().sum();
    assert!(need > 2, "уровней ≥ 2: {need} страниц");
    let ids: Vec<PageId> = (0..need as u64).collect();
    let pages = build(&items, &ids).unwrap();
    assert_eq!(pages.len(), need);
    // спуск из корня (последняя страница) по маршруту каждого ключа:
    // интерьеры ведут, лист отдаёт ровно его запись
    for (k, mt) in &items {
        let mut p = need as PageId - 1;
        loop {
            let node = parse_node(&pages[p as usize]);
            if node.level == 0 {
                let gmt = leaf_find(&node, k).expect("ключ есть");
                assert_eq!(gmt, mt.as_slice());
                break;
            }
            p = route(&node, k).expect("маршрут ведёт");
        }
    }
    // ключа нет: ниже наименьшего маршрут не ведёт, выше наибольшего —
    // лист молчит
    let root = parse_node(&pages[need - 1]);
    assert!(route(&root, &[0, 0]).is_none());
    let p = route(&root, &[0xFF, 0xFF, 0, 0]).unwrap();
    let leaf = parse_node(&pages[p as usize]);
    assert_eq!(leaf.level, 0);
    assert!(leaf_find(&leaf, &[0xFF, 0xFF, 0, 0]).is_none());
    // запись тяжелее страницы — громкий отказ до всякой записи
    assert!(plan(&[(vec![0u8; PAGE_SIZE], vec![])]).is_err());
}

#[test]
fn sb_roundtrip() {
    let e0 = ChainEntry { page: 12, off: 5 };
    let e1 = ChainEntry::default();
    let tables = vec![(b"a".to_vec(), 7), (b"cache:Repo:v3".to_vec(), 12)];
    let mut p = AlignedPage::zeroed();
    write_sb(&mut p, 9, 42, 43, &tables, e0, e1, 100, 0, 0xDEAD, 0xBEEF);
    let sb = parse_sb(&p).unwrap();
    assert_eq!((sb.r#gen, sb.next0, sb.next1), (9, 42, 43));
    assert_eq!(sb.tables, tables);
    assert_eq!(
        (sb.e0, sb.e1, sb.c0, sb.c1, sb.s0, sb.s1),
        (e0, e1, 100, 0, 0xDEAD, 0xBEEF)
    );
    assert!(parse_sb(&AlignedPage::zeroed()).is_none());
    assert!(parse_sb(&AlignedPage([TORN_BYTE; PAGE_SIZE])).is_none());
    // шум: гигантский счётчик таблиц зажат форматом страницы — запись
    // полного набора читается ровно, хвост не заходит
    let full: Vec<(Vec<u8>, PageId)> = (0..MAX_TABLES)
        .map(|i| (format!("t{i}").into_bytes(), 0))
        .collect();
    let mut junk = AlignedPage::zeroed();
    write_sb(&mut junk, 1, 4, 5, &full, e0, e1, 0, 0, 0, 0);
    junk.0[32..36].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(parse_sb(&junk).unwrap().tables.len(), MAX_TABLES);
}

#[test]
fn fnv_stable() {
    assert_eq!(fnv_id(&[]), 0xcbf29ce484222325);
    let h = 0xcbf29ce484222325u64
        .wrapping_mul(0x100000001B3)
        .wrapping_add(7);
    assert_eq!(fnv_id(&[7]), h);
}
