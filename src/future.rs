use std::cmp;
use std::error;
use std::fmt;
use std::future::Future;
use std::iter::{IntoIterator, Iterator};
use std::pin::Pin;
use std::task::{Context, Poll};

use pin_project::pin_project;
use tokio::time::{sleep_until, Duration, Instant, Sleep};

use crate::error::Error as RetryError;
use crate::notify::Notify;

use super::action::Action;
use super::condition::Condition;

#[pin_project(project = RetryStateProj)]
enum RetryState<A>
where
    A: Action,
{
    Running(#[pin] A::Future),
    Sleeping(#[pin] Sleep),
}

impl<A: Action> RetryState<A> {
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> RetryFuturePoll<A> {
        match self.project() {
            RetryStateProj::Running(future) => RetryFuturePoll::Running(future.poll(cx)),
            RetryStateProj::Sleeping(future) => RetryFuturePoll::Sleeping(future.poll(cx)),
        }
    }
}

enum RetryFuturePoll<A>
where
    A: Action,
{
    Running(Poll<Result<A::Item, RetryError<A::Error>>>),
    Sleeping(Poll<()>),
}

/// Future that drives multiple attempts at an action via a retry strategy.
#[pin_project]
pub struct Retry<I, A>
where
    I: Iterator<Item = Duration>,
    A: Action,
{
    #[pin]
    retry_if: RetryIf<I, A, fn(&A::Error) -> bool, fn(&A::Error, std::time::Duration)>,
}

impl<I, A> Retry<I, A>
where
    I: Iterator<Item = Duration>,
    A: Action,
{
    pub fn spawn<T: IntoIterator<IntoIter = I, Item = Duration>>(
        strategy: T,
        action: A,
    ) -> Retry<I, A> {
        Retry {
            retry_if: RetryIf::spawn(
                strategy,
                action,
                (|_| true) as fn(&A::Error) -> bool,
                (|_, _| {}) as fn(&A::Error, std::time::Duration),
            ),
        }
    }

    pub fn spawn_notify<T: IntoIterator<IntoIter = I, Item = Duration>, F>(
        strategy: T,
        action: A,
        notify: F,
    ) -> RetryIf<I, A, fn(&A::Error) -> bool, F>
    where
        F: FnMut(&A::Error, std::time::Duration),
    {
        RetryIf::spawn(
            strategy,
            action,
            (|_| true) as fn(&A::Error) -> bool,
            notify,
        )
    }
}

impl<I, A> Future for Retry<I, A>
where
    I: Iterator<Item = Duration>,
    A: Action,
{
    type Output = Result<A::Item, A::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = self.project();
        this.retry_if.poll(cx)
    }
}

/// Future that drives multiple attempts at an action via a retry strategy. Retries are only attempted if
/// the `Error` returned by the future satisfies a given condition.
#[pin_project]
pub struct RetryIf<I, A, C, N>
where
    I: Iterator<Item = Duration>,
    A: Action,
    C: Condition<A::Error>,
    N: Notify<A::Error>,
{
    strategy: I,
    #[pin]
    state: RetryState<A>,
    action: A,
    condition: C,
    duration: Duration,
    notify: N,
}

impl<I, A, C, N> RetryIf<I, A, C, N>
where
    I: Iterator<Item = Duration>,
    A: Action,
    C: Condition<A::Error>,
    N: Notify<A::Error>,
{
    pub fn spawn<T: IntoIterator<IntoIter = I, Item = Duration>>(
        strategy: T,
        mut action: A,
        condition: C,
        notify: N,
    ) -> RetryIf<I, A, C, N> {
        RetryIf {
            strategy: strategy.into_iter(),
            state: RetryState::Running(action.run()),
            action,
            condition,
            duration: Duration::from_millis(0),
            notify,
        }
    }

    fn attempt(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<A::Item, A::Error>> {
        let future = {
            let mut this = self.as_mut().project();
            this.action.run()
        };
        self.as_mut()
            .project()
            .state
            .set(RetryState::Running(future));
        self.poll(cx)
    }

    fn retry(
        mut self: Pin<&mut Self>,
        err: A::Error,
        cx: &mut Context,
    ) -> Result<Poll<Result<A::Item, A::Error>>, A::Error> {
        match self.as_mut().project().strategy.next() {
            None => {
                #[cfg(feature = "tracing")]
                tracing::warn!("ending retry: strategy reached its limit");
                Err(err)
            }
            Some(duration) => {
                *self.as_mut().project().duration += duration;
                let deadline = Instant::now() + duration;
                let future = sleep_until(deadline);
                self.as_mut()
                    .project()
                    .state
                    .set(RetryState::Sleeping(future));
                Ok(self.poll(cx))
            }
        }
    }
}

impl<I, A, C, N> Future for RetryIf<I, A, C, N>
where
    I: Iterator<Item = Duration>,
    A: Action,
    C: Condition<A::Error>,
    N: Notify<A::Error>,
{
    type Output = Result<A::Item, A::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        match self.as_mut().project().state.poll(cx) {
            RetryFuturePoll::Running(poll_result) => match poll_result {
                Poll::Ready(Ok(ok)) => Poll::Ready(Ok(ok)),
                Poll::Pending => Poll::Pending,
                Poll::Ready(Err(error)) => match error {
                    RetryError::Permanent(err) => Poll::Ready(Err(err)),
                    RetryError::Transient { err, retry_after } => {
                        if self.as_mut().project().condition.should_retry(&err) {
                            let duration =
                                retry_after.unwrap_or(self.as_ref().project_ref().duration.clone());
                            self.as_mut().project().notify.notify(&err, duration);
                            *self.as_mut().project().duration = duration;
                            match self.retry(err, cx) {
                                Ok(poll) => poll,
                                Err(err) => Poll::Ready(Err(err)),
                            }
                        } else {
                            Poll::Ready(Err(err))
                        }
                    }
                },
            },
            RetryFuturePoll::Sleeping(poll_result) => match poll_result {
                Poll::Pending => Poll::Pending,
                Poll::Ready(_) => self.attempt(cx),
            },
        }
    }
}
