use async_trait::async_trait;
use futures::stream::Stream;
use futures::{FutureExt, TryFutureExt};
use reqwest::header::HeaderMap;
use reqwest::{StatusCode, Url};
use scraper::Html;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as StdContext, Poll};
use std::time::Duration;

type Selection<T> = Pin<Box<dyn Future<Output = T>>>;

type HtmlRequest<T> = Pin<Box<dyn Future<Output = Result<Response<T>, reqwest::Error>>>>;

pub struct Collector<T: Selector> {
    /// Currently running jobs
    running_selections: Vec<Selection<T::Output>>,
    requests: Vec<HtmlRequest<T::UrlContext>>,
    selector: T,
    state: State<T::UrlContext>,
    /// Number of concurrent requests
    max: usize,
}

impl<T: Selector> Collector<T> {
    pub fn scrape() {}
}

impl<T> Stream for Collector<T>
where
    T: Selector + Unpin + 'static,
    <T as Selector>::Output: Unpin + 'static,
    <T as Selector>::UrlContext: Unpin + 'static,
{
    type Item = Result<T::Output, ()>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<Option<Self::Item>> {
        let pin = self.get_mut();

        // Drain pending selection processes
        for n in (0..pin.running_selections.len()).rev() {
            let mut selection = pin.running_selections.swap_remove(n);
            if let Poll::Ready(res) = selection.poll_unpin(cx) {
                return Poll::Ready(Some(Ok(res)));
            }
            pin.running_selections.push(selection);
        }

        while pin.requests.len() < pin.max {
            if let Some(url) = pin.state.pending.pop_front() {
                pin.requests.push(Box::pin(pin.state.get_html(url)));
            } else {
                break;
            }
        }

        unimplemented!()
    }
}

// Handler for a single job
// pub struct Job<T> {}

pub struct State<T> {
    requests: Vec<HtmlRequest<T>>,
    client: Arc<reqwest::Client>,
    current_req: (),
    pending: VecDeque<UrlInfo<T>>,
    stats: Stats,
    request_delay: Option<RequestDelay>,
}

impl<T: 'static + Unpin> State<T> {
    fn get_html(&self, info: UrlInfo<T>) -> impl Future<Output = reqwest::Result<Response<T>>> {
        self.client
            .get(info.url.clone())
            .send()
            .and_then(|mut response| {
                let status = response.status();
                let mut headers = HeaderMap::default();
                std::mem::swap(&mut headers, response.headers_mut());
                response.text().and_then(move |txt| {
                    let html = Html::parse_document(&txt);
                    futures::future::ok(Response {
                        status,
                        headers,
                        info,
                        html,
                    })
                })
            })
    }
}

struct UrlInfo<T> {
    ctx: T,
    url: Url,
    depth: usize,
}

pub struct Response<T> {
    html: Html,
    status: StatusCode,
    headers: HeaderMap,
    info: UrlInfo<T>,
}

pub struct Context<'a, T> {
    depth: usize,
    request_url: Url,
    response_status: StatusCode,
    response_headers: HeaderMap,
    state: &'a mut State<T>,
    pub url_context: T,
}

impl<'a, T> Context<'a, T> {
    pub fn visit_url_with_context(&self, url: Url, ctx: T) {}
}

impl<'a, T: Default> Context<'a, T> {
    pub fn visit_url(&self, url: Url) {
        self.visit_url_with_context(url, T::default())
    }
}

pub struct UrlValidator {
    allowed_domains: Vec<String>,
    filters: Vec<Box<dyn UrlFilter>>,
    robots_map: Vec<()>,
}

impl UrlValidator {
    fn is_valid_url(&self, url: &Url) -> bool {
        true
    }
}

pub trait UrlFilter {
    fn is_valid(&self, url: &Url) -> bool;
}

impl<F> UrlFilter for F
where
    F: Fn(&Url) -> bool,
{
    fn is_valid(&self, url: &Url) -> bool {
        (self)(url)
    }
}

#[cfg(feature = "re")]
impl UrlFilter for regex::Regex {
    fn is_valid(&self, url: &Url) -> bool {
        self.is_match(url.as_str())
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    request_count: usize,
    response_count: usize,
}

#[async_trait(?Send)]
pub trait Selector {
    /// The Output type this Selector produces for a response
    type Output;
    /// Used to add additional context to an url that should be requested
    type UrlContext;

    async fn on_document(
        &mut self,
        doc: Html,
        ctx: Context<Self::UrlContext>,
    ) -> Option<Self::Output>;

    /// This checks whether a submitted url should in fact be requested
    fn is_valid_url(&self, url: &Url) -> bool {
        true
    }
}

#[derive(Debug)]
pub struct CollectorConfig {
    /// Limits the recursion depth of visited URLs.
    max_depth: Option<usize>,
    /// Whether to allow multiple downloads of the same URL
    allow_url_revisit: bool,
    /// Whether to ignore responses with a non 2xx response code see
    /// `reqwest::Response::is_success`
    skip_http_error_response: bool,
    /// allows the Collector to ignore any restrictions set by the target host's
    /// robots.txt file.
    ignore_robots_txt: bool,
    /// Limit the retrieved response body in bytes.
    response_body_limit: Option<u64>,
    /// Delay a request
    request_delay: Option<RequestDelay>,
}

/// How to delay request
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
}
