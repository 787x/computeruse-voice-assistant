//! 取消令牌。
//!
//! 需求：用户取消后，迟到的 LLM / PowerShell 响应**不得**触发新的工具调用，
//! 也不得覆盖新回合的界面状态。
//!
//! 做法：每个回合持有一个 `CancelToken`；`Agent` 在**每一次**跨 await 的边界
//! （发请求前、收流中、执行工具前、工具返回后）都检查它。
//! 取消是单向的：一旦取消，令牌永久处于取消态，迟到的结果只能被丢弃。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::Notify;

#[derive(Debug)]
struct Inner {
    cancelled: AtomicBool,
    notify: Notify,
}

/// 可克隆的取消令牌（克隆共享同一个取消状态）。
#[derive(Debug, Clone)]
pub struct CancelToken {
    inner: Arc<Inner>,
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelToken {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                cancelled: AtomicBool::new(false),
                notify: Notify::new(),
            }),
        }
    }

    pub fn cancel(&self) {
        if !self.inner.cancelled.swap(true, Ordering::SeqCst) {
            self.inner.notify.notify_waiters();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// 等到被取消为止（已取消则立即返回）。
    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        // 先登记再复查：避免 cancel() 恰好落在两次检查之间导致永远等待。
        let notified = self.inner.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }

    /// 若已取消则返回 `Err(Cancelled)`，用于在每个 await 边界后统一短路。
    pub fn check(&self) -> Result<(), Cancelled> {
        if self.is_cancelled() {
            Err(Cancelled)
        } else {
            Ok(())
        }
    }
}

/// 回合已被取消。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("已中断")]
pub struct Cancelled;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancel_is_sticky_and_shared() {
        let token = CancelToken::new();
        let clone = token.clone();
        assert!(!token.is_cancelled());
        clone.cancel();
        assert!(token.is_cancelled());
        assert!(clone.is_cancelled());
        token.check().unwrap_err();
    }

    #[tokio::test]
    async fn cancelled_wakes_waiters() {
        let token = CancelToken::new();
        let waiter = token.clone();
        let handle = tokio::spawn(async move {
            waiter.cancelled().await;
            true
        });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        token.cancel();
        assert!(handle.await.unwrap());
    }

    #[tokio::test]
    async fn cancelled_returns_immediately_when_already_cancelled() {
        let token = CancelToken::new();
        token.cancel();
        tokio::time::timeout(std::time::Duration::from_millis(50), token.cancelled())
            .await
            .expect("已取消的令牌必须立即返回");
    }
}
