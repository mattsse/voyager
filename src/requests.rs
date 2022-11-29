use futures::stream::Stream;
use futures::Future;
use futures_timer::Delay;
use reqwest::header::HeaderMap;
use reqwest::{StatusCode, Url};
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

/// A [Request](reqwest::Request) that is waiting to be sent.
pub struct QueuedRequest<T> {
    /// The request to send.
    pub request: reqwest::Request,
    /// The requested state to use when crawling.
    pub state: Option<T>,
    /// How many links the crawler visited before it queued this request.
    pub depth: usize,
}

/// Builder for [QueuedRequest].
pub struct QueuedRequestBuilder<T> {
    /// The request to send.
    pub request: reqwest::RequestBuilder,
    /// The requested state to use when crawling.
    pub state: Option<T>,
    /// How many links the crawler visited before it queued this request.
    pub depth: usize,
}

/// Queue containing the list of requests left to crawl.
///
/// This queue does not dedupe requests.
pub struct RequestQueue<T> {
    delay: Option<(Delay, RequestDelay)>,
    queued_requests: VecDeque<QueuedRequest<T>>,
}

impl<T> RequestQueue<T> {
    /// Creates a new [RequestQueue] with the given delay.
    pub fn with_delay(delay: RequestDelay) -> Self {
        Self {
            delay: Some((Delay::new(Duration::default()), delay)),
            queued_requests: Default::default(),
        }
    }

    pub fn queue_mut(&mut self) -> &mut VecDeque<QueuedRequest<T>> {
        &mut self.queued_requests
    }

    /// Removes the crawl delay.
    ///
    /// Requests will be crawled immediately where possible.
    pub fn remove_delay(&mut self) -> Option<RequestDelay> {
        self.delay.take().map(|(_, d)| d)
    }

    /// Set a delay to be applied between requests.
    pub fn set_delay(&mut self, mut delay: RequestDelay) -> Option<RequestDelay> {
        if let Some((_, d)) = self.delay.as_mut() {
            std::mem::swap(&mut delay, d);
            Some(delay)
        } else {
            self.delay = Some((Delay::new(Duration::default()), delay));
            None
        }
    }

    pub fn is_empty(&self) -> bool {
        self.queued_requests.is_empty()
    }

    pub fn len(&self) -> usize {
        self.queued_requests.len()
    }
}

impl<T> Default for RequestQueue<T> {
    fn default() -> Self {
        Self {
            delay: None,
            queued_requests: Default::default(),
        }
    }
}

impl<T: Unpin> Stream for RequestQueue<T> {
    type Item = QueuedRequest<T>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.queued_requests.is_empty() {
            return Poll::Ready(None);
        }
        let pin = self.get_mut();
        let mut next = None;
        if let Some((mut delay, dur)) = pin.delay.take() {
            if Delay::poll(Pin::new(&mut delay), cx).is_ready() {
                next = pin.queued_requests.pop_front();
                delay.reset(dur.next_delay());
            }
            pin.delay = Some((delay, dur));
        } else {
            next = pin.queued_requests.pop_front()
        }
        Poll::Ready(next)
    }
}

/// How to delay a request
#[derive(Debug, Clone, Copy)]
pub enum RequestDelay {
    /// Apply a fixed delay to request
    Fixed(Duration),
    /// Apply a random delay to a request that is in the range of (`min`..`max`)
    Random {
        /// minimum delay duration to apply
        min: Duration,
        /// maximum delay duration to apply
        max: Duration,
    },
}

impl RequestDelay {
    /// Use a fixed delay
    pub fn fixed(delay: Duration) -> Self {
        RequestDelay::Fixed(delay)
    }

    /// Use a random delay of range `0`..max`
    pub fn random(max: Duration) -> Self {
        RequestDelay::Random {
            min: Duration::from_millis(0),
            max,
        }
    }

    /// Use a random delay of range `min`..max`
    pub fn random_in_range(min: Duration, max: Duration) -> Self {
        RequestDelay::Random { min, max }
    }

    /// Computes the next [Duration] of delay that the [RequestQueue] should take.
    ///
    /// For [Fixed](RequestDelay::Fixed) delays, this will return the fixed value.
    ///
    /// For [Random](RequestDelay::Random) delays, it will choose a new random number.
    pub fn next_delay(&self) -> Duration {
        use rand::Rng;

        match self {
            RequestDelay::Fixed(delay) => *delay,
            RequestDelay::Random { min, max } => Duration::from_millis(
                rand::thread_rng().gen_range(min.as_millis() as u64..=max.as_millis() as u64),
            ),
        }
    }
}

/// Returns a tuple containing information about the [Response](reqwest::Response).
///
/// This helps callers avoid accidentally moving the [Response](reqwest::Response)
/// when reading its sub-fields.
pub(crate) fn response_info(resp: &mut reqwest::Response) -> (StatusCode, Url, HeaderMap) {
    let mut headers = HeaderMap::new();
    std::mem::swap(&mut headers, resp.headers_mut());
    (resp.status(), resp.url().clone(), headers)
}
