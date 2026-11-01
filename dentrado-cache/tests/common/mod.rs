//! Общие помощники ужесточённых тестов: гонка фьючерса с бюджетом
//! оборотов планировщика — отмена в await-точке (штатный режим вызова
//! под select/timeout) и дедлайн-проба (зависание — громкий падёж, не
//! таймаут CI). Тесты подключают как `mod common;`.

use std::future::Future;
use std::task::Poll;

/// Гонка фьючерса с `n` оборотами планировщика (self-wake — каждый
/// оборот полный цикл опроса). `Some` — фьючерс успел; `None` — лимит
/// застал его живым в await-точке, и он отброшен: это и отмена
/// (cancel-at-k), и индикатор зависания для пробы-дедлайна.
pub async fn race_turns<F: Future>(f: F, n: u32) -> Option<F::Output> {
    let mut left = n;
    let ticker = futures::future::poll_fn(|cx| {
        if left == 0 {
            Poll::Ready(())
        } else {
            left -= 1;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    });
    futures::pin_mut!(f);
    futures::pin_mut!(ticker);
    match futures::future::select(f, ticker).await {
        futures::future::Either::Left((r, _)) => Some(r),
        futures::future::Either::Right(..) => None,
    }
}

/// Проба-дедлайн: фьючерс обязан закончиться в бюджет оборотов —
/// зависание (потерянное пробуждение, брошенная очередь) падает громко
/// с указанием точки.
pub async fn expect_turns<F: Future>(f: F, n: u32, what: &str) -> F::Output {
    race_turns(f, n)
        .await
        .unwrap_or_else(|| panic!("зависание: {what} (бюджет {n} оборотов)"))
}
