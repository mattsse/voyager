use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use anyhow::{Error, Result};
use futures::stream::Stream;
use futures::{Future, FutureExt, TryFutureExt};
use reqwest::header::HeaderMap;

use crate::error::{CrawlError, DisallowReason};
use crate::requests::{
    response_info, QueuedRequest, QueuedRequestBuilder, RequestDelay, RequestQueue,
};
use crate::response::Response;
use crate::robots::{RobotsData, RobotsHandler};

pub enum DomainListing<T> {
    AllowList(AllowList<T>),
    BlockList(BlockList<T>),
}

impl<T> DomainListing<T>
where
    T: Unpin + Send + Sync + fmt::Debug + 'static,
{
    pub fn add_request(&mut self, request: QueuedRequestBuilder<T>) -> Result<(), CrawlError<T>> {
        let QueuedRequestBuilder {
            request,
            state,
            depth,
        } = request;

        match request.build() {
            Ok(request) => {
                let queued = QueuedRequest {
                    request,
                    state,
                    depth,
                };
                match self {
                    DomainListing::AllowList(list) => Ok(list.add_request(queued)?),
                    DomainListing::BlockList(list) => Ok(list.add_request(queued)?),
                }
            }
            Err(error) => {
                return Err(CrawlError::<T>::FailedToBuildRequest {
                    error,
                    state,
                    depth,
                });
            }
        }
    }
}

impl<T> Stream for DomainListing<T>
where
    T: Unpin + Send + Sync + fmt::Debug + 'static,
{
    type Item = Result<Response<T>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.get_mut() {
            DomainListing::AllowList(list) => Stream::poll_next(Pin::new(list), cx),
            DomainListing::BlockList(list) => Stream::poll_next(Pin::new(list), cx),
        }
    }
}

pub struct AllowList<T> {
    /// All queued results that are gathered from all the different futures
    ///
    /// polling them all consecutively and buffering all the results prevents
    /// bias
    queued_results: VecDeque<Result<Response<T>>>,
    /// all allowed domains
    allowed: HashMap<String, AllowedDomain<T>>,
    domains: Vec<String>,
}

impl<T> Default for AllowList<T> {
    fn default() -> Self {
        Self {
            queued_results: Default::default(),
            allowed: Default::default(),
            domains: Vec::new(),
        }
    }
}

impl<T> AllowList<T> {
    pub fn allow(&mut self, domain: String, config: AllowListConfig) {
        self.allowed
            .insert(domain.clone(), AllowedDomain::new(config));
        self.domains.push(domain)
    }

    pub fn disallow(&mut self, domain: &str) -> Option<AllowedDomain<T>> {
        if let Some(list) = self.allowed.remove(domain) {
            let idx = self.domains.iter().position(|d| d == domain).unwrap();
            self.domains.remove(idx);
            Some(list)
        } else {
            None
        }
    }
}

impl<T> AllowList<T>
where
    T: Unpin + Send + Sync + fmt::Debug + 'static,
{
    pub fn add_request(&mut self, req: QueuedRequest<T>) -> Result<(), CrawlError<T>> {
        if let Some(host) = req.request.url().host_str() {
            if let Some(allowed) = self.allowed.get_mut(host) {
                allowed.add_request(req);
                Ok(())
            } else {
                Err(CrawlError::DisallowedRequest {
                    request: req.request,
                    state: req.state,
                    reason: DisallowReason::User,
                })
            }
        } else {
            Err(CrawlError::InvalidRequest {
                request: req.request,
                state: req.state,
            })
        }
    }
}

impl<T> Stream for AllowList<T>
where
    T: Unpin + Send + Sync + fmt::Debug + 'static,
{
    type Item = Result<Response<T>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let pin = self.get_mut();

        loop {
            if let Some(res) = pin.queued_results.pop_front() {
                return Poll::Ready(Some(res));
            }
            let mut busy = false;
            for n in (0..pin.domains.len()).rev() {
                let domain = pin.domains.swap_remove(n);
                let (key, mut list) = pin.allowed.remove_entry(&domain).unwrap();
                loop {
                    match Stream::poll_next(Pin::new(&mut list), cx) {
                        Poll::Ready(Some(res)) => {
                            pin.queued_results.push_back(res);
                        }
                        Poll::Pending => {
                            busy = true;
                            break;
                        }
                        _ => break,
                    }
                }
                pin.allowed.insert(key, list);
                pin.domains.push(domain);
            }

            if pin.queued_results.is_empty() {
                return if busy {
                    Poll::Ready(None)
                } else {
                    Poll::Pending
                };
            }
        }
    }
}

type CrawlRequest<T> = Pin<Box<dyn Future<Output = Result<Response<T>>>>>;

type RobotsTxtRequest = Pin<Box<dyn Future<Output = Result<RobotsData>>>>;

pub struct AllowedDomain<T> {
    client: Arc<reqwest::Client>,
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
    /// Respect any restrictions set by the target host's robots.txt file
    respect_robots_txt: bool,
}

impl<T> AllowedDomain<T> {
    pub fn new(config: AllowListConfig) -> Self {
        Self {
            client: config.client,
            in_progress_crawl_requests: Vec::new(),
            in_progress_robots_txt_crawls: None,
            request_queue: config
                .delay
                .map(RequestQueue::with_delay)
                .unwrap_or_default(),
            tmp_request_buffer: None,
            robots: None,
            skip_non_successful_responses: config.skip_non_successful_responses,
            respect_robots_txt: config.respect_robots_txt,
        }
    }

    pub fn add_request(&mut self, req: QueuedRequest<T>) {
        if self.respect_robots_txt && self.robots.is_none() {
            if self.in_progress_robots_txt_crawls.is_none() {
                // add request to fetch robots.txt
                let mut url = req.request.url().clone();
                url.set_path("robots.txt");
                let fut = self.client.get(url).send();
                let fut = Box::pin(async move {
                    let resp = fut.await?;
                    Ok(RobotsHandler::from_response(resp).await?)
                });
                self.in_progress_robots_txt_crawls = Some(fut);
            }
            // robots not ready yet
            let buf = self.tmp_request_buffer.get_or_insert(VecDeque::default());
            buf.push_back(req);
        } else {
            self.request_queue.queue_mut().push_back(req);
        }
    }
}

impl<T> Stream for AllowedDomain<T>
where
    T: Unpin + Send + Sync + fmt::Debug + 'static,
{
    type Item = Result<Response<T>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let pin = self.get_mut();

        // fetch robots.txt
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
            // queue in all requests that arrived while robots.txt was being fetched
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
                pin.request_queue.queue_mut().extend(tmp.into_iter());
            }
        }

        while let Poll::Ready(Some(req)) = Stream::poll_next(Pin::new(&mut pin.request_queue), cx) {
            if pin
                .robots
                .as_ref()
                .map(|robots| robots.is_not_disallowed(&req.request))
                .unwrap_or(true)
            {
                // respect robots.txt
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

        // advance all http requests
        for n in (0..pin.in_progress_crawl_requests.len()).rev() {
            let mut request = pin.in_progress_crawl_requests.swap_remove(n);

            if let Poll::Ready(resp) = request.poll_unpin(cx) {
                return Poll::Ready(Some(resp));
            } else {
                pin.in_progress_crawl_requests.push(request);
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

pub struct AllowListConfig {
    pub delay: Option<RequestDelay>,
    pub respect_robots_txt: bool,
    pub client: Arc<reqwest::Client>,
    pub skip_non_successful_responses: bool,
}

pub struct BlockList<T> {
    client: Arc<reqwest::Client>,
    /// list of domains that are blocked
    blocked_domains: HashSet<String>,
    /// Futures that eventually return a http response that is passed to the
    /// scraper
    in_progress_crawl_requests: Vec<CrawlRequest<T>>,
    /// mapping of hosts to robots.txt data
    robots_map: HashMap<String, RobotsData>,
    /// stores the futures that request the robots txt for a host
    in_progress_robots_txt_crawls: Vec<(String, RobotsTxtRequest)>,
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

impl<T> BlockList<T> {
    pub fn new(
        blocked_domains: HashSet<String>,
        client: Arc<reqwest::Client>,
        respect_robots_txt: bool,
        skip_non_successful_responses: bool,
    ) -> Self {
        BlockList {
            client,
            blocked_domains,
            in_progress_crawl_requests: Vec::new(),
            robots_map: Default::default(),
            in_progress_robots_txt_crawls: Vec::new(),
            in_progress_robots_txt_crawl_hosts: Default::default(),
            respect_robots_txt,
            skip_non_successful_responses,
            request_queue: Default::default(),
        }
    }
}

impl<T> BlockList<T>
where
    T: Unpin + Send + Sync + fmt::Debug + 'static,
{
    pub fn add_request(&mut self, req: QueuedRequest<T>) -> Result<(), CrawlError<T>> {
        if let Some(host) = req.request.url().host_str() {
            if self.blocked_domains.contains(host) {
                Err(CrawlError::DisallowedRequest {
                    request: req.request,
                    state: req.state,
                    reason: DisallowReason::User,
                })
            } else {
                self.request_queue.queue_mut().push_back(req);
                Ok(())
            }
        } else {
            Err(CrawlError::InvalidRequest {
                request: req.request,
                state: req.state,
            })
        }
    }
}

impl<T> Stream for BlockList<T>
where
    T: Unpin + Send + Sync + fmt::Debug + 'static,
{
    type Item = Result<Response<T>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let pin = self.get_mut();

        // drive all robots.txt to completion
        for n in (0..pin.in_progress_robots_txt_crawls.len()).rev() {
            let (host, mut fut) = pin.in_progress_robots_txt_crawls.swap_remove(n);
            if let Poll::Ready(result) = fut.poll_unpin(cx) {
                match result {
                    Ok(robots) => {
                        pin.in_progress_robots_txt_crawl_hosts.remove(&host);
                        pin.robots_map.insert(host, robots);
                    }
                    Err(_) => {
                        // robots.txt lookup failed
                        pin.in_progress_robots_txt_crawl_hosts.remove(&host);
                        return Poll::Ready(Some(Err(
                            CrawlError::<T>::RobotsTxtError { host }.into()
                        )));
                    }
                }
            } else {
                pin.in_progress_robots_txt_crawls.push((host, fut))
            }
        }

        for _ in 0..pin.request_queue.len() {
            if let Poll::Ready(Some(req)) = Stream::poll_next(Pin::new(&mut pin.request_queue), cx)
            {
                if pin.respect_robots_txt {
                    if let Some(host) = req.request.url().host_str() {
                        if let Some(robots) = pin.robots_map.get(host) {
                            if robots.is_not_disallowed(&req.request) {
                                let fut = Box::pin(get_response(
                                    &pin.client,
                                    req,
                                    pin.skip_non_successful_responses,
                                ));
                                pin.in_progress_crawl_requests.push(fut);
                            } else {
                                return Poll::Ready(Some(Err(CrawlError::DisallowedRequest {
                                    request: req.request,
                                    state: req.state,
                                    reason: DisallowReason::RobotsTxt,
                                }
                                .into())));
                            }
                        } else {
                            let mut url = req.request.url().clone();
                            url.set_path("robots.txt");
                            let fut = pin.client.get(url).send();

                            let fut = Box::pin(async move {
                                let resp = fut.await?;
                                Ok(RobotsHandler::from_response(resp).await?)
                            });

                            pin.in_progress_robots_txt_crawls
                                .push((host.to_string(), fut));
                            pin.in_progress_robots_txt_crawl_hosts
                                .insert(host.to_string());
                            pin.request_queue.queue_mut().push_back(req);
                        }
                    } else {
                        return Poll::Ready(Some(Err(CrawlError::InvalidRequest {
                            request: req.request,
                            state: req.state,
                        }
                        .into())));
                    }
                } else {
                    let fut = Box::pin(get_response(
                        &pin.client,
                        req,
                        pin.skip_non_successful_responses,
                    ));
                    pin.in_progress_crawl_requests.push(fut);
                }
            }
        }

        for n in (0..pin.in_progress_crawl_requests.len()).rev() {
            let mut request = pin.in_progress_crawl_requests.swap_remove(n);

            if let Poll::Ready(resp) = request.poll_unpin(cx) {
                return Poll::Ready(Some(resp));
            } else {
                pin.in_progress_crawl_requests.push(request);
            }
        }

        if pin.in_progress_crawl_requests.is_empty()
            && pin.request_queue.is_empty()
            && pin.in_progress_robots_txt_crawls.is_empty()
        {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }
}

fn get_response<T>(
    client: &reqwest::Client,
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
