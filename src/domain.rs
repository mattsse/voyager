use crate::error::{CrawlError, DisallowReason};
use crate::requests::{QueuedRequest, QueuedRequestBuilder, RequestDelay, RequestQueue};
use crate::response::Response;
use crate::robots::{RobotsData, RobotsHandler};
use anyhow::{Error, Result};
use futures::stream::Stream;
use futures::{Future, FutureExt, TryFutureExt};
use reqwest::header::HeaderMap;
use reqwest::{StatusCode, Url};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

pub enum DomainListing<T> {
    WhiteList(HashMap<String, WhiteListed<T>>),
    BlackList(BlackListed<T>),
}

impl<T> DomainListing<T> {
    pub fn add_request(&mut self, request: QueuedRequestBuilder<T>) -> Result<()> {
        todo!()
    }
}

impl<T> Stream for DomainListing<T> {
    type Item = Result<Response<T>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        unimplemented!()
    }
}

type CrawlRequest<T> = Pin<Box<dyn Future<Output = Result<Response<T>>>>>;

type RobotsTxtRequest = Pin<Box<dyn Future<Output = Result<RobotsData>>>>;

pub struct WhiteListed<T> {
    client: Arc<reqwest::Client>,
    /// The base url for the domain
    base_url: Url,
    /// Futures that eventually return a http response that is passed to the
    /// scraper
    in_progress_crawl_requests: Vec<CrawlRequest<T>>,
    /// stores the future that requests the robots txt for a host
    in_progress_robots_txt_crawls: Option<RobotsTxtRequest>,
    /// Temporary buffer for requests that may arrive while robots.txt is being
    /// fetched
    tmp_request_buffer: Option<VecDeque<QueuedRequest<T>>>,
    /// The request in process to fetch the the robots.txt
    /// all queued requests
    request_queue: RequestQueue<T>,
    /// The parsed robots.txt contents of this domain
    robots: Option<RobotsData>,
    /// Whether to ignore responses with a non 2xx response code see
    /// `reqwest::Response::is_success`
    skip_non_successful_responses: bool,
}

impl<T> WhiteListed<T> {
    pub fn new(config: WhiteListConfig) -> Self {
        Self {
            client: config.client,
            base_url: config.base_url,
            in_progress_crawl_requests: Vec::new(),
            in_progress_robots_txt_crawls: config.with_robots,
            request_queue: config
                .delay
                .map(RequestQueue::with_delay)
                .unwrap_or_default(),
            tmp_request_buffer: None,
            robots: None,
            skip_non_successful_responses: config.skip_non_successful_responses,
        }
    }
}

impl<T: Unpin + Send + Sync + fmt::Debug + 'static> Stream for WhiteListed<T> {
    type Item = Result<Response<T>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let pin = self.get_mut();

        if let Some(mut fut) = pin.in_progress_robots_txt_crawls.take() {
            if let Poll::Ready(result) = fut.poll_unpin(cx) {
                match result {
                    Ok(data) => {
                        pin.robots = Some(data);
                    }
                    Err(err) => return Poll::Ready(Some(Err(err))),
                }
            } else {
                pin.in_progress_robots_txt_crawls = Some(fut);
                return Poll::Pending;
            }
        }

        if let Some(mut tmp) = pin.tmp_request_buffer.take() {
            if let Some(robots) = pin.robots.take() {
                while let Some(req) = tmp.pop_front() {
                    if robots.is_not_disallowed(&req.request) {
                        pin.request_queue.queue_mut().push_back(req);
                    } else {
                        pin.robots = Some(robots);
                        pin.tmp_request_buffer = Some(tmp);
                        return Poll::Ready(Some(Err(CrawlError::DisallowedRequest {
                            request: req.request,
                            state: req.state,
                            reason: DisallowReason::RobotsTxt,
                        }
                        .into())));
                    }
                }
                pin.robots = Some(robots);
            } else {
                // robots request failed
                pin.tmp_request_buffer = Some(tmp);
                return Poll::Pending;
            }
        }

        while let Poll::Ready(Some(req)) = Stream::poll_next(Pin::new(&mut pin.request_queue), cx) {
            if pin
                .robots
                .as_ref()
                .map(|robots| robots.is_not_disallowed(&req.request))
                .unwrap_or(true)
            {
                let mut fut = Box::pin(get_response(
                    &pin.client,
                    req,
                    pin.skip_non_successful_responses,
                ));
                if let Poll::Ready(resp) = fut.poll_unpin(cx) {
                    return Poll::Ready(Some(resp));
                } else {
                    pin.in_progress_crawl_requests.push(fut);
                }
            } else {
                return Poll::Ready(Some(Err(CrawlError::DisallowedRequest {
                    request: req.request,
                    state: req.state,
                    reason: DisallowReason::RobotsTxt,
                }
                .into())));
            }
        }

        for n in (0..pin.in_progress_crawl_requests.len()).rev() {
            let mut request = pin.in_progress_crawl_requests.swap_remove(n);

            if let Poll::Ready(resp) = request.poll_unpin(cx) {
                return Poll::Ready(Some(resp));
            }
        }
        if pin.in_progress_crawl_requests.is_empty()
            && pin.in_progress_robots_txt_crawls.is_none()
            && pin.request_queue.is_empty()
        {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }
}

pub struct WhiteListConfig {
    base_url: Url,
    delay: Option<RequestDelay>,
    with_robots: Option<RobotsTxtRequest>,
    client: Arc<reqwest::Client>,
    skip_non_successful_responses: bool,
}

pub struct BlackListed<T> {
    client: Arc<reqwest::Client>,
    /// Domain blacklist
    disallowed_domains: HashSet<String>,
    /// Futures that eventually return a http response that is passed to the
    /// scraper
    in_progress_crawl_requests: Vec<CrawlRequest<T>>,
    /// mapping of hosts to robots.txt data
    robots_map: HashMap<String, RobotsData>,
    /// stores the futures that request the robots txt for a host
    in_progress_robots_txt_crawls: Vec<RobotsTxtRequest>,
    /// Currently in progress robot txt crawl request
    in_progress_robots_txt_crawl_hosts: HashSet<String>,
    /// Respect any restrictions set by the target host's robots.txt file
    respect_robots_txt: bool,
    /// Whether to ignore responses with a non 2xx response code see
    /// `reqwest::Response::is_success`
    skip_non_successful_responses: bool,
    /// all queued requests
    request_queue: RequestQueue<T>,
}

fn response_info(resp: &mut reqwest::Response) -> (StatusCode, Url, HeaderMap) {
    let mut headers = HeaderMap::new();
    std::mem::swap(&mut headers, resp.headers_mut());
    (resp.status(), resp.url().clone(), headers)
}

/// queue in a request to fetch the robots txt from the url's host
async fn get_robots_txt(client: &Arc<reqwest::Client>, mut url: Url) -> Result<RobotsData> {
    url.set_path("robots.txt");
    let resp = client.get(url).send().await?;
    Ok(RobotsHandler::from_response(resp).await?)
}

fn get_response<T>(
    client: &Arc<reqwest::Client>,
    request: QueuedRequest<T>,
    skip_non_successful_responses: bool,
) -> CrawlRequest<T>
where
    T: Unpin + Send + Sync + fmt::Debug + 'static,
{
    let QueuedRequest {
        request,
        state,
        depth,
    } = request;
    let request_url = request.url().clone();
    let skip_http_error_response = skip_non_successful_responses;

    let request = client.execute(request);

    Box::pin(async move {
        let mut resp = request.await?;

        if !resp.status().is_success() && skip_http_error_response {
            // skip unsuccessful response
            return Err(CrawlError::NoSuccessResponse {
                request_url: Some(request_url),
                response: resp,
                state,
            }
            .into());
        }

        let (status, url, headers) = response_info(&mut resp);

        let text = resp.text().await?;

        Ok(Response {
            depth,
            request_url,
            response_url: url,
            response_status: status,
            response_headers: headers,
            text,
            state,
        })
    })
}
