use std::convert::Infallible;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::response::sse::Event;
use tokio::sync::{broadcast, mpsc};

pub(super) struct EventStream {
    receiver: mpsc::Receiver<Result<Event, Infallible>>,
}

impl futures_core::Stream for EventStream {
    type Item = Result<Event, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(context)
    }
}

impl EventStream {
    pub(super) fn subscribe(
        mut receiver: broadcast::Receiver<String>,
        event: &'static str,
        capacity: usize,
    ) -> Self {
        let (sender, stream) = mpsc::channel(capacity);
        tokio::spawn(async move {
            loop {
                let payload = tokio::select! {
                    biased;
                    _ = sender.closed() => break,
                    result = receiver.recv() => match result {
                        Ok(payload) => payload,
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                };
                if sender
                    .send(Ok(Event::default().event(event).data(payload)))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        Self { receiver: stream }
    }
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;
    use tokio::time::{timeout, Duration};

    use super::*;

    #[tokio::test]
    async fn disconnect_releases_subscription_without_waiting_for_the_next_event() {
        let (sender, receiver) = broadcast::channel(8);
        let stream = EventStream::subscribe(receiver, "status", 4);
        assert_eq!(sender.receiver_count(), 1);
        drop(stream);
        timeout(Duration::from_secs(1), async {
            while sender.receiver_count() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn broadcast_shutdown_closes_the_http_stream() {
        let (sender, receiver) = broadcast::channel(8);
        let mut stream = EventStream::subscribe(receiver, "meter", 8);
        drop(sender);
        assert!(timeout(Duration::from_secs(1), stream.next())
            .await
            .unwrap()
            .is_none());
    }
}
